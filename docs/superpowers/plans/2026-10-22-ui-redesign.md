# UI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the GPUI UI around a photo-first, glass-like library with an adaptive grid, a collapsible icon rail, a toggleable inspector, a full-screen image viewer with a toggleable detail panel, and restrained motion.

**Architecture:** Keep `crates/ui/src/main.rs` as the single UI file. Replace the token values, add pure layout/motion/color helpers with unit tests, then layer the new shell, sidebar, grid, inspector, and full-screen viewer on top of the existing screens. Move the old detail screen body into a reusable `detail_body` rendered by the viewer. Add three persisted settings fields (sidebar, inspector, reduced motion) and a demo fixture generator so the visual result can be verified end to end.

**Tech Stack:** Rust 2024, gpui-kit 0.8.0 / gpui-pre 0.3.9, SQLite via `vrc_photo_core`, `image` 0.25.

**Spec:** `docs/superpowers/specs/2026-10-22-ui-redesign-design.md`

## Global Constraints

- Target is the GPUI UI (`crates/ui/src/main.rs`). Tauri migration #29 is withdrawn.
- Do not add a direct `gpui` dependency or bump `gpui-kit = "=0.8.0"`.
- Do not split `crates/ui/src/main.rs` into modules in this work.
- Photo grid stays `uniform_list` with fixed row height; only the column count varies.
- No element-level backdrop blur. Use an upscaled thumbnail + scrim for the inspector and a dark background for the full-screen viewer.
- No CSS `scale`/`rotate` transforms; animate opacity, width, and small offsets only.
- All `Animation::new(...)` calls use `.with_easing(ease_motion)` unless the animator already applies `stagger_value` (which applies the easing itself).
- New `AppSettings` fields must use `#[serde(default)]` so old JSON still deserializes.
- Do not trigger image loads or DB queries from animation closures.
- Keep existing import, search, compression, monitor, settings, tag, favorite, and trash behavior.
- Windows visual verification is deferred to #9; #27 requires Linux screenshots at 1280×820 and 1440×900.
- Do not mix docs and code in one PR. The `docs/ui.md` update is a separate docs-only PR.
- Use a worktree for each PR branch; never push directly to `main`.

## Review Focus

These are the failure modes most likely to bite a user and are easy to miss in a visual redesign. Each has a test or a manual check in its owning task.

1. Existing settings JSON without `ui_sidebar_collapsed` / `ui_reduce_motion` must not reset the entire settings record. (Task 2)
2. Resizing across 1280/1440, toggling the rail, and toggling the inspector must recompute columns without stale `uniform_list` rows, panics, or thumbnail re-decodes. (Tasks 6, 7, 10)
3. Space/Enter while typing in the search or settings inputs must not open the viewer; `Ctrl/Cmd+B`, `Ctrl/Cmd+I`, and `Ctrl/Cmd+K` must still work. (Tasks 4, 8, 10)
4. Missing preview/thumbnail/demo files must show a placeholder, not a panic; trash failures must surface a message. (Tasks 3, 6, 8)
5. The viewer must return focus to the grid after Esc/image-area click/close, and repeated open/close or details toggles must not leave focus stranded. (Task 8)
6. Reduced motion must render the final state immediately and schedule no animation frames. (Task 9)
7. The first-24-card intro must not replay on every scroll or re-decode thumbnails. (Tasks 6, 10)
8. The demo fixture must not write into the user's real thumbnail cache or collide with real photo IDs. (Task 3)
9. The inspector toggle (header button and `Ctrl+I`) must stay in sync with the persisted state and leave no blank column or stale grid when hidden. (Tasks 4, 7, 10)
10. The full-screen viewer must close only on Esc, the close button, or a click on the dark area around the image — not on the image or details panel. (Task 8)
11. `Ctrl/Cmd+B`, `Ctrl/Cmd+I`, and `Ctrl/Cmd+K` must be ignored while the viewer is open. (Tasks 4, 8)
12. Viewer previous/next must use the list it was opened from (Photos or Search), clamp at the ends, and not steal Left/Right from the tag input. (Tasks 8, 12)

---

# PR1 — Shell, sidebar, photos, full-screen viewer

Branch: `feat/ui-redesign-core` in a new worktree cut from updated `main`.

## Task 1: Tokens and pure helpers

**Files:**
- Modify: `crates/ui/src/main.rs` (top constants ~28-41; helper area ~2327-2369; tests at end of file)

**Interfaces:**
- Produces:
  - `const BORDER_STRONG: u32`
  - `const ACCENT_SOFT: u32`
  - `const SIDEBAR_EXPANDED: f32 = 238.0`
  - `const SIDEBAR_COLLAPSED: f32 = 78.0`
  - `const MIN_CARD_WIDTH: f32 = 170.0`
  - `const GRID_GAP: f32 = 16.0`
  - `const INSPECTOR_WIDTH: f32 = 330.0`
  - `const VIEWER_DETAILS_WIDTH: f32 = 380.0`
  - `const WORKSPACE_PAD_X: f32 = 44.0`
  - `const CARD_INTRO_SECS: f32 = 0.6`
  - `const CARD_STAGGER_SECS: f32 = 0.04`
  - `const CARD_STAGGER_MAX: usize = 8`
  - `fn sidebar_width(collapsed: bool) -> f32`
  - `fn sidebar_reveal_width(collapsed: bool, t: f32) -> f32`
  - `fn grid_available_width(window_width: f32, sidebar_collapsed: bool, inspector_collapsed: bool) -> f32`
  - `fn viewer_details_width(open: bool, t: f32) -> f32`
  - `fn grid_columns(available_width: f32) -> usize`
  - `fn format_count(value: i64) -> String`
  - `fn average_bgra(bytes: &[u8]) -> u32`
  - `fn lerp_color(from: u32, to: u32, t: f32) -> u32`
  - `fn ease_motion(t: f32) -> f32`
  - `fn stagger_value(elapsed: f32, index: usize) -> f32`
  - `fn next_focus_index(current: usize, len: usize, reverse: bool) -> usize`
  - `fn step_index(current: usize, len: usize, delta: isize) -> usize`
  - `fn parse_window_size(value: &str) -> Option<(f32, f32)>`

- [ ] **Step 1: Replace the palette values and add the new tokens**

Keep every existing constant name (`BG`, `PANEL`, `PANEL_2`, `SIDEBAR`, `BORDER`, `TEXT`, `MUTED`, `MUTED_2`, `BLUE`, `BLUE_DARK`, `GREEN`, `AMBER`, `RED`, `PURPLE`) so later screens inherit the palette without renames. Set the values from the design table. Add `BORDER_STRONG` and `ACCENT_SOFT`.

- [ ] **Step 2: Add the pure helpers with their exact signatures**

Implement:

```rust
fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed { SIDEBAR_COLLAPSED } else { SIDEBAR_EXPANDED }
}

fn sidebar_reveal_width(collapsed: bool, t: f32) -> f32 {
    let (from, to) = if collapsed { (130.0, 0.0) } else { (0.0, 130.0) };
    from + (to - from) * t
}

fn grid_available_width(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> f32 {
    let inspector = if inspector_collapsed { 0.0 } else { INSPECTOR_WIDTH };
    (window_width - sidebar_width(sidebar_collapsed) - inspector - WORKSPACE_PAD_X).max(0.0)
}

fn viewer_details_width(open: bool, t: f32) -> f32 {
    let (from, to) = if open { (0.0, VIEWER_DETAILS_WIDTH) } else { (VIEWER_DETAILS_WIDTH, 0.0) };
    from + (to - from) * t
}

fn grid_columns(available_width: f32) -> usize {
    (((available_width + GRID_GAP) / (MIN_CARD_WIDTH + GRID_GAP)).floor() as usize).max(1)
}
```

`grid_available_width` subtracts the inspector column only when it is not collapsed. `viewer_details_width` interpolates 0..380 for the full-screen details panel. `format_count` rules: `< 1000` plain; `1000..=9999` comma-separated; `>= 10_000` compact with one decimal below 100k (`12.8k`) and no decimal at 100k+ (`100k`). `average_bgra` treats input as BGRA, ignores alpha `< 8`, and returns `0xRRGGBB`; empty/transparent input returns `0`. `lerp_color` interpolates each channel with `.round()` and clamps `t` to 0..1. `ease_motion` solves the cubic Bezier `(0.22, 0.8, 0.24, 1)` with 8 Newton iterations and clamps the result to 0..1. `stagger_value` uses `delay = min(index, 8) * 0.04`, `total = 0.6 + delay`, `local = ((elapsed * total - delay) / 0.6).clamp(0.0, 1.0)`, then `ease_motion(local)`. `next_focus_index` wraps in both directions and returns 0 for `len == 0`. `step_index` clamps the current index into `0..len`, applies the signed delta with saturation, and clamps to the ends; it returns 0 for `len == 0`. `parse_window_size` accepts `<width>x<height>` or `X`, requires both numbers, and rejects dimensions below 320×240.

- [ ] **Step 3: Add the failing tests**

Append a `#[cfg(test)] mod tests` at the end of `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
    fn window_size_parser_accepts_wxh() {
        assert_eq!(parse_window_size("1440x900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("1440X900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("0x900"), None);
        assert_eq!(parse_window_size("1440,900"), None);
        assert_eq!(parse_window_size("1440x"), None);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vrc-photo-ui`
Expected: all 11 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add UI redesign tokens and helpers"
```

## Task 2: Persist sidebar, inspector, and reduced-motion settings

**Files:**
- Modify: `crates/core/src/settings.rs` (~10-45, tests ~197-223)

**Interfaces:**
- Produces: `AppSettings.ui_sidebar_collapsed: bool`, `AppSettings.ui_inspector_collapsed: bool`, and `AppSettings.ui_reduce_motion: bool`, all defaulting to `false` and all annotated `#[serde(default)]`.

- [ ] **Step 1: Write the backward-compatibility test**

Add to the `settings.rs` test module:

```rust
#[test]
fn settings_deserialize_without_ui_fields() {
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
    assert!(!settings.ui_sidebar_collapsed);
    assert!(!settings.ui_inspector_collapsed);
    assert!(!settings.ui_reduce_motion);
}
```

- [ ] **Step 2: Run it to confirm failure**

Run: `cargo test -p vrc-photo-core settings_deserialize_without_ui_fields`
Expected: FAIL with "missing field `ui_sidebar_collapsed`" or a compile error.

- [ ] **Step 3: Add the fields and defaults**

In the struct:

```rust
#[serde(default)]
pub ui_sidebar_collapsed: bool,
#[serde(default)]
pub ui_inspector_collapsed: bool,
#[serde(default)]
pub ui_reduce_motion: bool,
```

In `Default`: all three `false`. Do not add a struct-level `#[serde(default)]`; keep the change scoped to the new fields.

- [ ] **Step 4: Run the settings tests**

Run: `cargo test -p vrc-photo-core settings`
Expected: PASS, including the existing roundtrip test.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs
git commit -m "feat: persist sidebar, inspector, and reduced-motion preferences"
```

## Task 3: Launch options and demo fixtures

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` ~66-101; `new` ~104-189; `load_thumbnails` ~309-319; `open_database` ~2185-2213; `seed_demo` ~2214-2300; `main` ~2405-2429)

**Interfaces:**
- Produces:
  - `#[derive(Clone)] struct LaunchOptions { background: bool, demo: bool, demo_reset: bool, window_size: (f32, f32) }`
  - `fn parse_args(args: impl IntoIterator<Item = String>) -> LaunchOptions`
  - `struct DemoAssets { photo_dir: PathBuf, thumb_dir: PathBuf }`
  - `fn prepare_demo(reset: bool) -> Result<(Db, DemoAssets), String>`
  - `fn demo_thumb_path(assets: &DemoAssets, photo_id: i64) -> PathBuf`
  - `open_database(options: &LaunchOptions) -> (Db, bool, Option<String>, Option<DemoAssets>)`
- `App` gains `demo_assets: Option<DemoAssets>`.
- `seed_demo` becomes `seed_demo(db: &Db, photo_dir: &Path) -> Result<(), String>` and writes real files.

- [ ] **Step 1: Add `LaunchOptions` and `parse_args`**

Rules:
- `--background`, `--demo`, `--demo-reset` are booleans.
- `--window-size WxH` consumes the next argument; invalid/missing values fall back to `(1280.0, 820.0)`.
- Unknown arguments are ignored, but never consume the next argument.
- Defaults: `background = false`, `demo = false`, `demo_reset = false`, `window_size = (1280.0, 820.0)`.

Add tests:

```rust
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
```

- [ ] **Step 2: Implement the demo fixture generator**

`prepare_demo(reset)`:
- `let root = std::env::temp_dir().join("vrc-photo-manager-demo");`
- If `reset`, remove `root` recursively.
- Create `root/photos` and `root/thumbnails`.
- Open `root/library.db` with `Db::open`. If `db.count_photos()? == 0`, call `seed_demo(&db, &root.join("photos"))`.
- Return `DemoAssets` and the opened `Db`.

`seed_demo`:
- Keep the 18 rows, worlds, players, tags, favorites, and derivatives from the current function.
- Write a deterministic 1280×800 PNG per row with `image::RgbaImage`:
  - vertical gradient between two RGB colors chosen from a fixed 8-entry palette,
  - a filled sun circle at `(x = 70% width, y = 30% height, r = 90)`,
  - a dark silhouette across the bottom 25% using a simple per-column height formula.
- Save as `photo_dir/VRChat_demo_{index}.png`.
- Insert the photo with the real path, `width: 1280`, `height: 800`, and the actual file size.
- After insertion, load the PNG, `.thumbnail(256, 256)`, and save it as `thumb_dir/{id}.png` (PNG, not WebP).
- Return `Ok(())` on success; propagate I/O/image errors as `String`.

`demo_thumb_path(assets, photo_id)` returns `assets.thumb_dir.join(format!("{photo_id}.png"))`.

- [ ] **Step 3: Wire demo thumbnails into `load_thumbnails`**

Use `self.demo_assets.as_ref().map(|assets| demo_thumb_path(assets, photo.id))` when present; otherwise keep `thumbnail::thumbnail_path(photo.id)`. Do not write demo thumbnails into the real cache.

- [ ] **Step 4: Wire launch options into `open_database` and `main`**

`open_database(&options)` uses `options.demo`/`options.demo_reset`:
- Demo: `prepare_demo(options.demo_reset)` returns the opened DB and assets; return `(db, true, None, Some(assets))`.
- If demo preparation fails, fall back to an empty in-memory DB and return a message. Do not panic.
- Non-demo: keep the current DB path. If the real DB cannot be opened, fall back to `prepare_demo(true)` and return `(db, true, Some(message), Some(assets))`; if that also fails, use an empty in-memory DB with the message. Return `None` for assets only when no demo files were prepared.

`main`:
- Parse args once: `let options = parse_args(std::env::args());`
- If `options.background`, run the supervisor and return.
- Set `WindowOptions`:
  - `window_bounds: Some(WindowBounds::centered(size(px(w), px(h)), cx))`
  - `window_min_size: Some(size(px(1080.), px(700.)))`
- Pass `options` into the window closure and `App::new(window, cx, &options)`.

- [ ] **Step 5: Run the tests and a demo smoke test**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo --demo-reset`
Expected: window opens, 18 generated photos are visible with thumbnails, and no files are written under the real thumbnail cache path.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add demo fixtures and window sizing"
```

## Task 4: Sidebar/search state and keyboard bindings

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` ~66-101; `new` ~104-189; `refresh_monitor` ~282-308; `render` ~2167-2183; `main` ~2405-2429)

**Interfaces:**
- Produces on `App`:
  - `sidebar_collapsed: bool`
  - `sidebar_motion_epoch: u64`
  - `inspector_collapsed: bool`
  - `inspector_epoch: u64`
  - `hovered_nav: Option<Screen>`
  - `daemon_age_secs: Option<i64>`
- Produces actions: `ToggleSidebar`, `ToggleInspector`, `FocusSearch` (namespace `ui`).
- Produces methods: `toggle_sidebar(&mut self, cx: &mut Context<Self>)`, `toggle_inspector(&mut self, cx: &mut Context<Self>)`, `focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>)`.

- [ ] **Step 1: Add the state and initialize it**

In `App::new`:
- `sidebar_collapsed = initial_settings.ui_sidebar_collapsed`
- `sidebar_motion_epoch = 0`
- `inspector_collapsed = initial_settings.ui_inspector_collapsed`
- `inspector_epoch = 0`
- `hovered_nav = None`
- `daemon_age_secs = None`

In `refresh_monitor`, after reading `daemon_heartbeat`, compute `daemon_age_secs = (now - timestamp).max(0)` and keep `daemon_active = age < 20`.

- [ ] **Step 2: Add the actions and handlers**

```rust
gpui_kit::actions!(ui, [ToggleSidebar, ToggleInspector, FocusSearch]);
```

`toggle_sidebar` flips the flag, increments `sidebar_motion_epoch`, writes `ui_sidebar_collapsed` into `self.settings`, saves, and calls `cx.notify()`.

`toggle_inspector` flips `inspector_collapsed`, increments `inspector_epoch`, writes `ui_inspector_collapsed`, saves, and notifies. It is ignored while the full-screen viewer is open (Task 8).

`focus_search` sets `self.screen = Screen::Search`, focuses `self.world_input` via `window.focus(&self.world_input.read(cx).focus_handle(cx), cx)`, and notifies. If the input is not yet in the focus tree, defer the focus with `window.defer(cx, ...)`.

- [ ] **Step 3: Register the bindings**

In `main`, after `gpui_kit::init(cx)`:

```rust
cx.bind_keys([
    KeyBinding::new("ctrl-b", ToggleSidebar, None),
    KeyBinding::new("cmd-b", ToggleSidebar, None),
    KeyBinding::new("ctrl-i", ToggleInspector, None),
    KeyBinding::new("cmd-i", ToggleInspector, None),
    KeyBinding::new("ctrl-k", FocusSearch, None),
    KeyBinding::new("cmd-k", FocusSearch, None),
]);
```

Attach handlers on the root element in `render`:

```rust
.on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
.on_action(cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)))
```

- [ ] **Step 4: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: `Ctrl+B` and `Ctrl+I` update the persisted flags without crashing yet (the visual changes land in Tasks 5 and 7); `Ctrl+K` switches to Search and focuses the world input.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add sidebar/inspector state and keyboard actions"
```

## Task 5: Collapsible icon-rail sidebar

**Files:**
- Modify: `crates/ui/src/main.rs` (`sidebar` ~427-498; `nav_item` ~500-530; `render` ~2167-2183)

**Interfaces:**
- Consumes `sidebar_width`, `SIDEBAR_EXPANDED`, `SIDEBAR_COLLAPSED`, `IconName`, `toggle_sidebar`.
- Produces: `fn nav_item(&mut self, label: &'static str, screen: Screen, icon: IconName, count: Option<String>, cx: &mut Context<Self>) -> AnyElement`
- Produces: `fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement`
- Produces: `fn rail_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement` (root-level overlay, painted after the workspace)
- Produces shared style helpers:
  - `fn glass_panel() -> Div` — `rgba(0x141922c4)` fill, `rgba(0xffffff14)` border, radius 18, drop shadow + inset top highlight.
  - `fn chip(label: &str, active: bool) -> Div` — pill, 10 px text, `ACCENT_SOFT`/`BLUE` when active, `PANEL_2`/`MUTED` otherwise.

- [ ] **Step 1: Add the shared surface helpers**

Implement `glass_panel()` and `chip()` as free functions near the other UI helpers. Use `BoxShadow::new(px(0.), px(24.), rgba(0x00000060)).blur_radius(px(60.))` plus an inset `BoxShadow::new(px(0.), px(1.), rgba(0xffffff0d)).inset().blur_radius(px(0.))`. These helpers are reused by the inspector, the viewer, and PR2 screens.

- [ ] **Step 2: Replace the sidebar shell**

Build a `v_flex` with:
- `.relative()`, `.h_full()`, `.py(px(16.))`, `.px(px(12.))`, `.gap(px(3.))`
- `.bg(rgb(SIDEBAR))`, `.border_r_1()`, `.border_color(rgba(0xffffff14))`
- width set by the animation wrapper, not the inner content.

Wrap the whole sidebar with:

```rust
.with_animation(
    ("sidebar", self.sidebar_motion_epoch),
    Animation::new(if self.sidebar_motion_epoch == 0 {
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
```

Use `into_any_element()` for the return type. Do not include the toggle inside the sidebar; it is rendered by `rail_toggle` at the root.

- [ ] **Step 3: Add the brand and floating toggle**

- Brand: 34 px rounded square with "VP", `BLUE`→`PURPLE` gradient background; text block "VRC Photo" / "Manager · {count}枚". Wrap the text block with a `with_animation` keyed by `SharedString::from(format!("sidebar-brand-{epoch}"))`, using `sidebar_reveal_width(collapsed, t)` for width and `t`/`1-t` for opacity. Use `Duration::ZERO` when `epoch == 0` (startup) and 320 ms otherwise. Add `.overflow_hidden()`.
- Toggle: render it from `rail_toggle(cx)` as the last child of the root (the root must be `.relative()`), absolutely positioned at `top(px(18.))` and `left(px(animated_sidebar_width - 12.))`, 24×24, circular, `bg(rgb(0x131923))`, border `BORDER_STRONG`, chevron left/right. Wrap it in a 440 ms `with_animation` keyed by the same epoch so its left edge follows the sidebar. Use `Button` with `.icon(...)` and a tooltip ("サイドバーを最小化 (Ctrl+B)" / "サイドバーを展開 (Ctrl+B)"). Clicking calls `toggle_sidebar`.
- Do not set `overflow_hidden` on the sidebar; the rail nav tooltips must escape the 78 px width.

- [ ] **Step 4: Implement the five nav items**

Icons: `GalleryVerticalEnd`, `Search`, `Minimize`, `Eye`, `Settings`.

Each item is a `Stateful<Div>`:
- `.id(label)`, `.relative()`, `.h(px(40.))`, `.w_full()`, `.rounded(px(11.))`, `.cursor_pointer()`.
- Expanded: `.px(px(11.))`, `.gap(px(11.))`, icon 17 px, label 12 px, count 10 px mono right-aligned. Wrap the label+count block in a `with_animation` keyed by `SharedString::from(format!("sidebar-label-{label}-{epoch}"))`, using `sidebar_reveal_width(collapsed, t)` and opacity `t`/`1-t`; use `Duration::ZERO` when `epoch == 0`, 320 ms otherwise; add `.overflow_hidden()`.
- Collapsed: `.justify_center()`, no label/count.
- Active: `.bg(rgb(ACCENT_SOFT))`, text `#eaf4ff`, icon `BLUE`, plus an absolute 2×18 accent bar at `left(-1)` expanded / `left(5)` collapsed.
- Inactive: text `MUTED`; hover `bg(rgb(0x161d28))` and text `TEXT`.
- Counts: Photos = `format_count(self.stats.photo_count)`; Compression = running/pending jobs from `self.jobs` when non-zero; others `None`.
- `on_click` sets `self.screen` and notifies.
- Collapsed hover tooltip: use `self.hovered_nav` + `on_hover`. Render an absolute label at `left(px(56.))`, `top(px(8.))`, `px(px(8.))`, `py(px(5.))`, rounded 7, `bg(rgb(0x161c26))`, border `BORDER_STRONG`, text 10 px. Only when collapsed and hovered.

- [ ] **Step 5: Add the bottom blocks**

- Spacer `.flex_1()`.
- Storage card (expanded only): `PANEL` surface, 13 px rounded, 11/12 px padding, "ライブラリ" label, `{count} 枚`, `format_bytes(stats.total_bytes)`. No disk meter.
- Status row: 6 px mint dot + "監視中" when `daemon_active`, else muted dot + "デーモン停止". Hide the text when collapsed.
- Version row: `DEMO DATA` when `self.demo_assets.is_some()`, else `env!("CARGO_PKG_VERSION")` prefixed with `v`.

- [ ] **Step 6: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: toggle collapses to 78 px and expands to 238 px; `Ctrl+B` does the same; labels/counts/storage hide in rail mode; hovering an icon shows its label; restarting preserves the state (demo DB is file-backed now).

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add collapsible icon rail sidebar"
```

## Task 6: Photos header, adaptive grid, and card redesign

**Files:**
- Modify: `crates/ui/src/main.rs` (`photos_screen` ~593-646; `photo_list` ~648-709; `photo_card` ~711-788; `load_thumbnails` ~309-319; `render` ~2167-2183)

**Interfaces:**
- Produces:
  - `enum PhotoCardLayout { Grid { index: usize }, Fixed }`
  - `fn photo_card(&mut self, photo: &Photo, layout: PhotoCardLayout, cx: &mut Context<Self>) -> AnyElement`
  - `fn photos_header(&mut self, cx: &mut Context<Self>) -> AnyElement`
- `App` gains `grid_columns: usize`, `grid_epoch: u64`, `thumbnail_colors: HashMap<i64, u32>`, `intro_seen: HashSet<i64>`, `photo_grid_focus: FocusHandle`.
- Produces `fn color_for(&self, photo: &Photo) -> u32`.

- [ ] **Step 1: Add the grid state and color extraction**

Create `photo_grid_focus = cx.focus_handle()` in `App::new` and call `window.focus(&photo_grid_focus, cx)` after the initial photo selection so Space/Enter work immediately (defer it if the grid is not yet in the focus tree). In `load_thumbnails`, after `load_render` succeeds:
```rust
let color = image
    .as_bytes(0)
    .map(average_bgra)
    .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")));
self.thumbnail_colors.insert(photo.id, color);
self.thumbnails.insert(photo.id, image);
```
Clear `thumbnail_colors` at the start. In `reload`, increment `grid_epoch` and clear `intro_seen` (new grid generation).

`color_for(photo)` returns `thumbnail_colors.get(&photo.id).copied().unwrap_or_else(|| scene_color(...))`.

- [ ] **Step 2: Compute columns during render**

In `Render::render`, before building the root:
```rust
let width: f32 = window.viewport_size().width.into();
self.grid_columns = grid_columns(grid_available_width(
    width,
    self.sidebar_collapsed,
    self.inspector_collapsed,
));
```
Keep the existing `topbar` for non-Photos screens. Skip it when `self.screen == Screen::Photos`; that screen renders its own `photos_header` inside `photos_screen`.

- [ ] **Step 3: Implement `photos_header`**

`photos_header` returns both the header row and the filter row in a `v_flex`. `photos_screen` returns a `v_flex` with `photos_header` on top and the grid + inspector row below.

- Height 72 px, `.px(px(26.))`, horizontal layout, bottom border.
- Left: "ライブラリ" at 24 px semibold; subtitle at 10 px `MUTED_2` with `format_count(photo_count) + " 枚 · " + format_bytes(total_bytes) + sync text`.
- Sync text: "デモデータ" when `demo_assets.is_some()`; "最終同期 N秒前" when `daemon_age_secs` is `Some(age)` and `age < 20`; "デーモン停止" otherwise.
- Message: `self.message` at 10 px `AMBER`, `.flex_1().min_w_0()`, ellipsis.
- Right: a "詳細" / "詳細を隠す" text toggle (calls `toggle_inspector`, tooltip `Ctrl+I`) and the "コピー取込" / "移動取込" text buttons (primary on move).
- Below the header, a filter row at 26 px horizontal padding, 14 px bottom:
  - Chips "ワールド" (active), "プレイヤー", "日時", "タグ" — static, no click handlers.
  - Right: "並び: 新しい順" at 10 px `MUTED_2`.

- [ ] **Step 4: Implement the adaptive `uniform_list`**

- Rows = `photos.len().div_ceil(columns)`.
- Row height `196.0`, gap 16.
- Each row: `h_flex().h(px(196.)).gap(px(16.))`.
- For each column, if a photo exists, call `photo_card(photo, PhotoCardLayout::Grid { index }, cx)`; otherwise add `div().flex_1()` to preserve card widths.
- Use `uniform_list(("photo-grid", columns), rows, ...)` or a formatted `SharedString` including columns so a column change resets the list state.
- Wrap the list in a container with `.id("photo-grid")`, `.track_focus(&self.photo_grid_focus)`, `.key_context("photo_grid")`, `.tab_stop(true)`.

- [ ] **Step 5: Implement `photo_card`**

Expanded grid card:
- `.id(SharedString::from(format!("photo-{id}")))`, `.relative()`, `.flex_1()`, `.min_w_0()`, `.gap(px(8.))`, `.cursor_pointer()`.
- Thumbnail area `.h(px(128.))`, rounded 14, border `BORDER`, `bg(scene_color)`, image `Cover`, a bottom `linear_gradient` scrim for caption legibility.
- Selected: 2 px `BLUE` border (or an outer ring via border + shadow) and a soft `BLUE` shadow.
- Badges: "圧縮済" mint dot/text when compressed; "未圧縮" amber dot/text when not; favorite amber dot at top-right.
- Meta: world name 12 px semibold with `.truncate()` (fallback "未紐付け"); second line 10 px `MUTED_2` with `short_date` + `format_bytes`.
- Click: `select_photo(id, cx)`, `window.focus(&self.photo_grid_focus, cx)`, and if `ClickEvent::Mouse(m)` has `m.down.click_count >= 2`, open the full-screen viewer (Task 8). Until Task 8, the double-click can only select.
- Hover: `on_hover` updates `self.hovered_photo`; wrap the card with:
```rust
let hovered = self.hovered_photo == Some(id);
.with_animation(
    SharedString::from(format!("card-hover-{id}-{hovered}")),
    Animation::new(Duration::from_millis(180)).with_easing(ease_motion),
    move |el, t| {
        let offset = if hovered { -4.0 * t } else { -4.0 * (1.0 - t) };
        el.relative().top(px(offset))
    },
)
```
- Intro: if `layout == Grid` and `self.intro_seen.insert(id)` and `index < 24`, wrap with:
```rust
.with_animation(
    SharedString::from(format!("card-intro-{grid_epoch}-{id}")),
    Animation::new(Duration::from_secs_f32(
        CARD_INTRO_SECS + index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS,
    ))
    .with_max_fps(60.0),
    move |el, t| {
        let value = stagger_value(t, index);
        el.opacity(value).relative().top(px(14.0 * (1.0 - value)))
    },
)
```
`Fixed` layout: `.w(px(190.))`, no intro. Apply the same hover animation.

- [ ] **Step 6: Update the Search screen call site**

`search_screen` currently calls `self.photo_card(photo, cx)`. Change it to `self.photo_card(photo, PhotoCardLayout::Fixed, cx).into_any_element()` so PR1 compiles. Do not restyle Search yet.

- [ ] **Step 7: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo --window-size 1280x820`
Expected: 3 columns with the sidebar expanded and the inspector visible; 4 columns with the sidebar collapsed; 5 columns with the inspector hidden. No thumbnail reload on animation.

Run: `cargo run -p vrc-photo-ui -- --demo --window-size 1440x900`
Expected: 4 columns expanded with the inspector visible; 5 columns with the sidebar collapsed; 6 columns with the inspector hidden.

- [ ] **Step 8: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: redesign photo grid with adaptive columns"
```

## Task 7: Photo-tinted glass inspector

**Files:**
- Modify: `crates/ui/src/main.rs` (`select_photo` ~333-340; `inspector` ~790-930; `load_selected` ~321-332)

**Interfaces:**
- `App` gains `selection_epoch: u64`, `previous_preview: Option<Arc<RenderImage>>`, `previous_color: Option<u32>`.
- Produces `fn selected_color(&self) -> u32`, `fn load_selected_meta(&mut self, id: i64)`. The inspector's detail action is rewired in Task 8.
- Produces `fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement` (the 314 px card) and `fn inspector_column(&mut self, cx: &mut Context<Self>) -> AnyElement` (the animated 330 px column).

- [ ] **Step 1: Split metadata loading from preview loading**

Add `load_selected_meta(id)` that updates `selected`, `selected_photo`, `selected_players`, `selected_tags`, `selected_derivatives` only. `load_selected(id)` calls it and then loads the preview. Use `load_selected_meta` for favorite and tag changes so the 1200 px preview is not re-decoded on metadata edits.

- [ ] **Step 2: Record the transition on selection**

In `select_photo`:
- If the id differs from the current selection, set `previous_preview = self.preview.clone()`, `previous_color = self.selected_photo.as_ref().map(|p| self.color_for(p))`, and increment `selection_epoch`.
- Then call `load_selected(id)`.

`selected_color()` returns `color_for(selected_photo)`.

- [ ] **Step 3: Rebuild the inspector**

- `inspector()` returns the 314 px card with 16 px top/bottom/right margins. `inspector_column()` wraps it in a 330 px column with `.h_full()` and `.overflow_hidden()`, and animates the column width:
```rust
let collapsed = self.inspector_collapsed;
.with_animation(
    ("inspector-column", self.inspector_epoch),
    Animation::new(if self.inspector_epoch == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(300)
    })
    .with_easing(ease_motion),
    move |el, t| {
        let from = if collapsed { INSPECTOR_WIDTH } else { 0.0 };
        let to = if collapsed { 0.0 } else { INSPECTOR_WIDTH };
        el.w(px(from + (to - from) * t))
    },
)
```
When `inspector_collapsed` is true, render the column empty (no card content); when false, render the card. `photos_screen` uses `inspector_column()`.
- The card is full height, rounded 18, `overflow_hidden`, `.relative()`.
- Background stack:
  1. Base solid color `rgb(selected_color())` (or the previous color during the transition).
  2. Selected photo's 256 px thumbnail, absolute at -24 px on all four edges, `ObjectFit::Cover`, opacity 0.45.
  3. Dark `linear_gradient(180.0, rgba(0x090d13cc), rgba(0x090d13f7))`.
  4. Content on top.
- Cross-fade:
  - Wrap the background/tint layers with `with_animation(("inspector-tint", selection_epoch), Animation::new(260ms), ...)`.
  - For the tint, use `lerp_color(previous_color.unwrap_or(current), current, t)`.
  - For the preview, stack `previous_preview` at opacity `1-t` and `self.preview` at opacity `t`. If there is no previous preview, fade the current in.
- Content:
  - Header: "選択中" uppercase 10 px `MUTED_2` + "Space で開く" 10 px.
  - Preview: 16:10, rounded 12, border, image `Cover`, shadow.
  - Title 17 px semibold; date 10 px mono `MUTED_2`; badges for context/favorite/compressed.
  - Meta rows: ワールド, インスタンス, 解像度, サイズ, 取得元, 撮影者. Update the shared `meta_row` helper to 10 px label/value with `.truncate()`.
  - Players and tags as pills (reuse the existing content).
  - Actions at the bottom: full-width "詳細を開く" primary (renamed to "全画面表示" in Task 8) and "圧縮" secondary, both text buttons. Keep the existing detail-screen action behind the first button so the intermediate commit is usable.
- Empty state: a quiet card with "写真を選択してください".

- [ ] **Step 4: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: selecting a photo animates the tint and preview; the inspector background visibly picks up the photo's palette; metadata edits do not flash/re-decode the preview; the header's "詳細" toggle and `Ctrl+I` collapse the column to 0 and expand it back, and the grid gains/loses columns.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add photo-tinted glass inspector"
```

## Task 8: Full-screen viewer and detail consolidation

**Files:**
- Modify: `crates/ui/src/main.rs` (`Screen` ~43-64; `App` ~66-101; `new` ~104-189; `trash_selected` ~341-373; `detail_screen` ~932-1192; `render` ~2167-2183)
- Remove: `Screen::Detail`, `Screen::Detail` title arm, `content` arm, and the old `detail_screen`.

**Interfaces:**
- `App` gains:
  - `quick_look_open: bool`
  - `quick_look_epoch: u64`
  - `quick_look_return_focus: Option<FocusHandle>`
  - `quick_look_focus_index: usize`
  - `quick_look_details_open: bool`
  - `quick_look_details_epoch: u64`
  - `quick_look_photo_ids: Vec<i64>`
  - `quick_look_index: usize`
  - focus handles `quick_look_close_focus`, `quick_look_prev_focus`, `quick_look_next_focus`, `quick_look_details_focus`, `quick_look_favorite_focus`, `quick_look_compress_focus`, `quick_look_trash_focus`, `quick_look_add_tag_focus`
- Produces:
  - `fn open_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `fn step_viewer_photo(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>)`
  - `fn viewer_source_ids(&self) -> Vec<i64>`
  - `fn toggle_quick_look_details(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `fn cycle_quick_look_focus(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>)`
  - `fn quick_look_focus_order(&self, cx: &App) -> Vec<FocusHandle>`
  - `fn detail_body(&mut self, photo: &Photo, cx: &mut Context<Self>) -> impl IntoElement`
  - `fn quick_look(&mut self, cx: &mut Context<Self>) -> AnyElement`
  - `fn focusable_text_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        focus: FocusHandle,
        primary: bool,
        cx: &mut Context<Self>,
        on_activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
    ) -> AnyElement`
- Actions: `OpenQuickLook`, `CloseQuickLook`, `ViewerPreviousPhoto`, `ViewerNextPhoto`, `QuickLookNextFocus`, `QuickLookPrevFocus`.

- [ ] **Step 1: Add the state, focus handles, and actions**

Create the eight focus handles in `App::new` with `cx.focus_handle()`. Initialize `quick_look_open = false`, `quick_look_epoch = 0`, `quick_look_return_focus = None`, `quick_look_focus_index = 0`, `quick_look_details_open = false`, `quick_look_details_epoch = 0`, `quick_look_photo_ids = Vec::new()`, `quick_look_index = 0`.

Extend the existing `gpui_kit::actions!(ui, [...])` block with the five viewer actions:

```rust
gpui_kit::actions!(ui, [
    ToggleSidebar,
    ToggleInspector,
    FocusSearch,
    OpenQuickLook,
    CloseQuickLook,
    ViewerPreviousPhoto,
    ViewerNextPhoto,
    QuickLookNextFocus,
    QuickLookPrevFocus,
]);
```

Add bindings:
```rust
KeyBinding::new("space", OpenQuickLook, Some("photo_grid")),
KeyBinding::new("enter", OpenQuickLook, Some("photo_grid")),
KeyBinding::new("escape", CloseQuickLook, Some("quick_look")),
KeyBinding::new("left", ViewerPreviousPhoto, Some("viewer_nav")),
KeyBinding::new("right", ViewerNextPhoto, Some("viewer_nav")),
KeyBinding::new("tab", QuickLookNextFocus, Some("quick_look")),
KeyBinding::new("shift-tab", QuickLookPrevFocus, Some("quick_look")),
```

Attach the handlers on the root element:
```rust
.on_action(cx.listener(|this, _: &OpenQuickLook, window, cx| this.open_quick_look(window, cx)))
.on_action(cx.listener(|this, _: &CloseQuickLook, window, cx| this.close_quick_look(window, cx)))
.on_action(cx.listener(|this, _: &ViewerPreviousPhoto, window, cx| this.step_viewer_photo(-1, window, cx)))
.on_action(cx.listener(|this, _: &ViewerNextPhoto, window, cx| this.step_viewer_photo(1, window, cx)))
.on_action(cx.listener(|this, _: &QuickLookNextFocus, window, cx| this.cycle_quick_look_focus(false, window, cx)))
.on_action(cx.listener(|this, _: &QuickLookPrevFocus, window, cx| this.cycle_quick_look_focus(true, window, cx)))
```

- [ ] **Step 2: Implement open/close, details toggle, and focus**

`open_quick_look`:
- Return if `self.selected_photo.is_none()` or already open.
- `self.quick_look_return_focus = window.focused(cx);`
- `self.quick_look_photo_ids = self.viewer_source_ids();`
- `self.quick_look_index = self.quick_look_photo_ids.iter().position(|id| Some(*id) == self.selected).unwrap_or(0);`
- `self.quick_look_open = true; self.quick_look_epoch += 1; self.quick_look_focus_index = 0;`
- Keep `quick_look_details_open` at its current value (initial false, then the session remembers the last toggle). Reset `quick_look_details_epoch = 0` so the remembered panel state renders immediately without an entrance animation; only explicit toggles animate.
- `window.focus(&self.quick_look_close_focus, cx);`
- `cx.notify();`

If the close button is not yet in the focus tree on the same frame, defer the focus with `window.defer(cx, |window, cx| window.focus(&close_focus, cx));`.

`viewer_source_ids()`:
- If `self.screen == Screen::Search` and `self.search_results` is `Some`, use those photo IDs.
- Otherwise use `self.photos` IDs.
- If the resulting list is empty or does not contain the selected ID, return `vec![selected_id]`.

`step_viewer_photo(delta, window, cx)`:
- Return if the viewer is closed or `quick_look_photo_ids` is empty.
- Compute `next = step_index(self.quick_look_index, self.quick_look_photo_ids.len(), delta)`.
- If `next == self.quick_look_index`, return (clamped at the end).
- Set `quick_look_index = next`, call `self.select_photo(self.quick_look_photo_ids[next], cx)`, and notify. The viewer stays open and the library selection updates behind it. Keep the current focus on the top-bar navigation button; do not move focus.

`close_quick_look`:
- `self.quick_look_open = false;`
- Restore `quick_look_return_focus.take()` with `window.focus(..., cx)`; if none, focus `self.photo_grid_focus`.
- `cx.notify();`

`toggle_quick_look_details`:
- Flip `quick_look_details_open`, increment `quick_look_details_epoch`.
- Always focus `self.quick_look_details_focus` so focus never remains inside a hidden panel.
- `cx.notify();`

`quick_look_focus_order(cx)` returns, in visual order: close, previous, next, details toggle, then, only when `quick_look_details_open`, favorite, `self.tag_edit_input.read(cx).focus_handle(cx)`, add-tag, compress, trash.

`cycle_quick_look_focus(reverse, window, cx)` uses `next_focus_index` and focuses the chosen handle. Update `quick_look_focus_index`.

Ignore `ToggleSidebar`, `ToggleInspector`, and `FocusSearch` while `quick_look_open` is true (early return in their handlers).

- [ ] **Step 3: Remove the Detail screen and extract `detail_body`**

- Delete `Screen::Detail` and its `title`, `content`, and sidebar-active references.
- Delete `detail_screen`.
- Create `detail_body(photo, cx)` from the old detail screen's right column, minus the back/header controls:
  - title at 24 px with `.truncate()`, favorite toggle, date at 10 px mono
  - meta rows (ワールド, インスタンス, 取得元, 撮影者, ファイル, 解像度, サイズ)
  - players, tags, tag input + add button
  - compression history
  - memo
  - compress and trash (two-step)
- Use `load_selected_meta` for favorite/tag updates.
- The tag-add button and favorite/compress/trash buttons must be focusable text buttons using the viewer focus handles.

- [ ] **Step 4: Implement the full-screen overlay**

`quick_look(cx)`:
- `let Some(photo) = self.selected_photo.clone() else { return div().into_any_element(); };`
- Root: `v_flex()` absolute at 0 on all four edges, `size_full`, `bg(rgb(0x05070a))`, `key_context("quick_look")`. Wrap the root in `with_animation(("viewer", self.quick_look_epoch), Animation::new(Duration::from_millis(240)).with_easing(ease_motion), |el, t| el.opacity(t))` for a fade-in. Add a full-window tint layer as the first child (behind the top bar and content row) at ~13% alpha using `rgba((color_for(&photo) << 8) | 0x22)` so the background picks up the photo's palette.
- Top bar: 56 px, `h_flex`, horizontal padding 20, bottom border `rgba(0xffffff12)`, translucent dark background, `.key_context("viewer_nav")`.
  - Left: "閉じる Esc" focusable text button using `quick_look_close_focus`.
  - Then "前へ" and "次へ" focusable text buttons using `quick_look_prev_focus` / `quick_look_next_focus`; they call `step_viewer_photo(-1/1, window, cx)`. Always render both; navigation clamps at the ends (muted at the boundary).
  - Middle: file name at 12 px `MUTED`, `.truncate()`, `.flex_1()`.
  - Right: "詳細" / "詳細を閉じる" focusable text button using `quick_look_details_focus`; calls `toggle_quick_look_details`.
- Content row: `h_flex().flex_1().min_h_0()`.
  - Image area: `flex_1`, `h_full`, `relative`, transparent over the tinted root, `.items_center().justify_center()`, `.p(px(24.))`, `.on_click` closes the viewer. Center an image container with `.aspect_ratio(if photo.height > 0 { photo.width as f32 / photo.height as f32 } else { 16.0 / 9.0 })`, `.max_w(relative(0.92))`, `.max_h(relative(0.92))`; wrap it in `with_animation(SharedString::from(format!("viewer-photo-{}-{}", photo.id, self.quick_look_epoch)), Animation::new(Duration::from_millis(220)).with_easing(ease_motion), |el, t| el.opacity(t))`. The 1200 px preview fills it with `ObjectFit::Contain`. Attach a no-op `on_click` to the image container so clicking the photo does not close the viewer, while clicking the letterbox (the image area) does. If `self.preview` is None, show a placeholder card tinted with `color_for(photo)` and the text "プレビューを読み込めません".
  - Details panel: always render the panel wrapper so the width can animate; render the `detail_body` content only when `quick_look_details_open`. Wrap the panel in `with_animation`:
```rust
let open = self.quick_look_details_open;
.with_animation(
    ("viewer-details", self.quick_look_details_epoch),
    Animation::new(if self.quick_look_details_epoch == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(300)
    })
    .with_easing(ease_motion),
    move |el, t| el.w(px(viewer_details_width(open, t))).overflow_hidden(),
)
```
The panel is `h_full`, `bg(rgba(0x0e131bdc))`, and contains a scrollable `detail_body(&photo, cx)`. Add 16 px padding and a left border only when open. When closed, width 0 and no content.
- The viewer stays open on favorite/tag/compress; trash closes it:
  - Trash handler: call `trash_selected(cx)`; if `self.selected.is_none()` after it, close the overlay.
  - Compression keeps the overlay open; the panel shows `self.message` above `detail_body`.

- [ ] **Step 5: Wire the entry points**

- Rename the inspector's primary action to "全画面表示" and wire its `on_click` to `open_quick_look`.
- Card double-click (Task 6): call `open_quick_look` when `click_count >= 2`.
- `Space`/`Enter` on the focused grid: action `OpenQuickLook`.
- Remove the old "詳細を開く" route.

- [ ] **Step 6: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected:
- Double-click, Space, and the inspector button open the full-screen viewer.
- "前へ" / "次へ" and Left/Right move through the current Photos list or Search results; they clamp at the first/last photo; the library selection follows. Left/Right do not navigate while the tag input is focused.
- The details panel starts closed; the top-bar "詳細" button opens/closes it; the image area expands when it closes.
- Clicking the dark area around the image closes the viewer; clicking the image or panel does not.
- Esc and the close button close it.
- Favorite, tag add, compress, and two-step trash work in the panel.
- Trash closes the viewer.
- Focus returns to the grid after closing, and is moved to the details toggle when the panel closes.
- Tab/Shift+Tab cycle close → previous → next → details → (when open) favorite → tag input → add-tag → compress → trash.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add full-screen viewer and consolidate photo detail"
```

## Task 9: Reduced-motion setting

**Files:**
- Modify: `crates/ui/src/main.rs` (`new` ~104-189; `settings_screen` ~1954-2067; `setting_switch` ~2069-2107)

**Interfaces:**
- Consumes `AppSettings.ui_reduce_motion`.
- Produces `fn set_reduce_motion(&mut self, enabled: bool, cx: &mut Context<Self>)`.

- [ ] **Step 1: Apply the setting at startup**

In `App::new`, after loading settings: `cx.set_reduce_motion(initial_settings.ui_reduce_motion);` before building the UI.

- [ ] **Step 2: Add the Settings switch**

Add a dedicated switch in the "起動と圧縮" card (or a new "表示" card):

- Label: "アニメーションを減らす"
- Description: "画面遷移とカードの動きを抑制します"
- Checked: `self.settings.ui_reduce_motion`
- On change: update the field, save, call `cx.set_reduce_motion(value)`, and notify.

Do not use OS-specific detection.

- [ ] **Step 3: Audit manual transitions**

Any motion that does not go through `with_animation` must check `cx.reduce_motion()` and skip the transition. In practice, the helpers in this plan all use `with_animation`; verify the sidebar, cards, inspector, and viewer all use it.

- [ ] **Step 4: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: toggling the switch makes the UI update instantly; restarting preserves the switch; no animation frames are scheduled when enabled.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add reduced-motion setting"
```

## Task 10: PR1 verification and screenshots

**Files:**
- No code changes expected; fix anything found.

- [ ] **Step 1: Run the full local gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all pass.

- [ ] **Step 2: Capture Linux screenshots**

Run each demo, find the window geometry, and capture it. Start from a clean demo state (`--demo-reset`) so the sidebar starts expanded:

```bash
cargo run -p vrc-photo-ui -- --demo --demo-reset --window-size 1280x820
hyprctl clients -j | jq '.[] | select(.title=="VRC Photo Manager") | {at, size}'
grim -g "<x>,<y> <w>x<h>" /tmp/opencode/ui-1280-expanded.png
```

Repeat for:
- 1280×820 collapsed (`Ctrl+B`)
- 1440×900 expanded
- 1440×900 collapsed
- inspector hidden (`Ctrl+I`)
- full-screen viewer with details closed
- full-screen viewer with details open
- Settings with reduced motion on

Attach the screenshots to the PR (do not commit them).

- [ ] **Step 3: Manual checklist**

- Sidebar toggle/persistence/Ctrl+B/tooltips.
- Inspector toggle/persistence/Ctrl+I; grid columns 3/5 at 1280 and 4/6 at 1440; +1 when the sidebar collapses.
- Card hover/intro; no thumbnail re-decode on scroll or resize.
- Inspector tint and preview cross-fade.
- Full-screen viewer open paths (double-click/Space/button), details toggle (top-bar button), previous/next at both ends, image-area click closes, focus return, tag/favorite/compress/trash.
- Space/Enter while typing in Search/Settings does not open the viewer.
- Search, compression, monitor, settings still work.
- Demo reset and window-size flags work.

- [ ] **Step 4: Open PR1**

```bash
git push -u origin feat/ui-redesign-core
gh pr create --title "feat: redesign GPUI shell, photo grid, and full-screen viewer" --body-file /tmp/opencode/pr1.md
```

PR body: summary, screenshots, test commands, Windows deferred to #9, `Closes #27` only if PR2 is merged in the same release (otherwise use `Refs #27`). Since PR2 completes the issue, use `Refs #27` in PR1 and `Closes #27` in PR2.

---

# PR2 — Remaining screens

Branch: `feat/ui-redesign-screens` in a new worktree cut from updated `main` after PR1 merges.

## Task 11: Compression screen token pass

**Files:**
- Modify: `crates/ui/src/main.rs` (`compression_screen` ~1194-1322; `profile_editor` ~1333-1557; `queue_panel` ~1559-1616; `stat_card` ~2109-2131; `badge` ~2155-2164)

- [ ] **Step 1: Apply the shared surfaces**

Replace the outer `p_5`/`gap_4` with 26 px horizontal padding and 24 px gaps. Use `glass_panel()` for profile cards and the queue panel. Use 12 px body, 10 px metadata, 17 px section titles. Add the existing `self.message` as an amber 10 px line in the compression header row so feedback remains visible after the global topbar is removed.

- [ ] **Step 2: Restyle the profile list**

Active profile: `ACCENT_SOFT` background, `BLUE` border, accent text. Inactive: `PANEL`, `BORDER`, `MUTED`. Keep the auto/manual badge colors.

- [ ] **Step 3: Restyle the editor and queue**

- Text buttons for format/resize/policy controls; keep the existing click handlers.
- Switch labels 12 px; descriptions 10 px.
- Queue rows: 10 px status badge, 12 px photo label, 10 px progress.
- Keep the existing warning callout, recolored to the token palette.

- [ ] **Step 4: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: profile selection, format, quality, resize, policy, and manual compression still work.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: apply redesign tokens to compression screen"
```

## Task 12: Search screen token pass

**Files:**
- Modify: `crates/ui/src/main.rs` (`search_screen` ~1618-1746; `filter_field` ~1840-1850; `preset_button` ~1817-1838; `policy_button` if shared)

- [ ] **Step 1: Apply the split layout**

- Left filter panel: 290 px, `PANEL` surface, 20 px rounded, 16 px padding, border `BORDER`.
- Right results area: 26 px padding, 24 px header, fixed-width photo cards (190 px) in a wrapping flex.
- Result header: 24 px count, 10 px "検索結果"; add the existing `self.message` as an amber 10 px line so feedback remains visible after the global topbar is removed.

- [ ] **Step 2: Restyle controls**

- Inputs keep their existing behavior; add 10 px labels.
- Preset buttons become chips: active `ACCENT_SOFT` + `BLUE` text, inactive `PANEL_2` + `MUTED`.
- Search/clear buttons become text buttons (primary/secondary).
- Keep the "結果は最大300枚です" note at 10 px.

- [ ] **Step 3: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: world/player/tag/date/favorite search still works; results show fixed-width cards; double-click opens the full-screen viewer (Space is validated on the Photos grid); previous/next stays within the Search results.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: apply redesign tokens to search screen"
```

## Task 13: Monitor and Settings token pass

**Files:**
- Modify: `crates/ui/src/main.rs` (`monitor_screen` ~1852-1952; `settings_screen` ~1954-2067; `setting_switch` ~2069-2107; `meta_row` ~2133-2144; `section_label` ~2146-2153)

**Interfaces:**
- Produces `fn screen_header(&self, title: &'static str, subtitle: String, message: Option<String>, actions: Vec<AnyElement>) -> AnyElement`.
- Consumes `glass_panel()` from Task 5.

- [ ] **Step 1: Apply the shared header and surfaces**

Replace each screen's local title row with `screen_header`:
- Monitor: "ログ監視", subtitle "VRChatの出力ログからワールドとプレイヤーを記録します。", reload action.
- Settings: "設定", subtitle "ライブラリと起動オプション", no actions.

Use `glass_panel()` for cards, 24 px gaps, 26 px horizontal padding.

- [ ] **Step 2: Restyle stat cards and rows**

- `stat_card`: 10 px label, 17 px value, 16 px padding, `PANEL` surface.
- `meta_row`: 86 px label column, 10 px label/value with `.truncate()`, `MUTED_2` label.
- `section_label`: 10 px uppercase with letter spacing (use `.text_size(px(10.))`; GPUI has no letter-spacing API, so use spacing between characters only if the API supports it — otherwise plain uppercase).
- Warning callout: `rgba(0x2a2412)` background, amber border/text.

- [ ] **Step 3: Remove the global topbar**

Photos, Monitor, and Settings now have their own headers; Compression keeps its in-body title and Search keeps its split header. Delete `topbar` and the temporary Photos special case. `render` becomes sidebar + workspace (the active screen's header + content).

- [ ] **Step 4: Verify**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

Run: `cargo run -p vrc-photo-ui -- --demo`
Expected: monitor refresh, settings path save, autostart/tray/auto-compress switches, and reduced-motion switch still work.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: apply redesign tokens to monitor and settings"
```

## Task 14: PR2 verification and screenshots

- [ ] **Step 1: Run the full local gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 2: Capture screenshots**

Repeat the 1280×820 / 1440×900 captures for Compression, Search, Monitor, and Settings. Attach them to PR2.

- [ ] **Step 3: Open PR2**

```bash
git push -u origin feat/ui-redesign-screens
gh pr create --title "feat: finish UI redesign token pass" --body-file /tmp/opencode/pr2.md
```

PR body: summary, screenshots, test commands, `Closes #27`.

---

# Follow-up (separate docs PR)

## Task 15: Document the redesigned UI

**Files:**
- Modify: `docs/ui.md`
- Add: `docs/ui-redesign-mockup.html` (copy from the `docs/ui-redesign-mockup` branch, commit `2bb7ce0`)

- [ ] **Step 1: Update `docs/ui.md`**

- Sidebar: 238/78 px, five icons, toggle, `Ctrl/Cmd+B`, persisted state.
- Photos: adaptive columns (3/5 at 1280, 4/6 at 1440 depending on inspector/sidebar state), 330 px inspector column, inspector show/hide, glass approximation.
- Full-screen viewer: replaces the detail screen; entry points; top-bar details toggle; previous/next buttons and Left/Right; focus behavior; actions.
- Inspector show/hide and `Ctrl/Cmd+I`.
- Keyboard shortcuts table.
- Motion: durations/easing, reduced-motion setting.
- Tokens: new palette, 8 px grid, 26 px outer padding, 10/12/17/24 type scale.
- Remove the old `Screen::Detail` description and the 210 px sidebar reference.
- Note that Windows visual verification is tracked in #9.

- [ ] **Step 2: Add the mock**

Copy `docs/ui-redesign-mockup.html` from the mock branch so the spec link is stable on `main`. Keep the file unmodified.

- [ ] **Step 3: Commit and open the docs-only PR**

```bash
git add docs/ui.md docs/ui-redesign-mockup.html
git commit -m "docs: document redesigned UI"
git push -u origin docs/ui-redesign
gh pr create --title "docs: document redesigned UI" --body "Updates docs/ui.md for #27 and adds the redesign mock. Refs #27."
```

---

## Self-Review Notes

- PR1 covers every acceptance item except the full token pass on Compression/Search/Monitor/Settings; those are PR2.
- PR1 removes the dedicated Detail screen and implements the specified full-screen viewer with a toggleable details panel, reusing `detail_body`.
- The `--demo` fixture is a QA aid, not a product feature; it must never write into the real thumbnail cache.
- `docs/ui.md` is deliberately a separate PR to respect the project's no-mixed-docs/code rule.
- The library browsing flow is now specified: the grid/inspector stay, the inspector toggles, and double-click opens the full-screen viewer with a toggleable details panel. Arrow-key navigation remains out of scope (#12).
