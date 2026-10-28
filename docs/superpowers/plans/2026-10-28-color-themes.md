# Color themes implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add two independent colour choices (main and accent, 18 options each) to Settings, with live application across the whole app and a single combined PR on top of `feat/ui-notifications`.

**Architecture:** A pure `Palette` value is derived from a main template (dark/light/pastel) plus hue/tint and an accent entry with contrast-safe tones. `App` owns the palette and passes it to render helpers; `gpui-component`'s theme is updated from the same value. Hardcoded colours are replaced by palette fields.

**Tech Stack:** Rust 2024, gpui-kit 0.8.1 (`gpui-component` 0.8.1), SQLite settings JSON.

**Spec:** `docs/superpowers/specs/2026-10-28-color-themes-design.md`

## Global Constraints

- Base branch is `feat/ui-notifications` (`33276fd`). One PR against `main` at the end contains both features; do not open an intermediate PR.
- **Never run heavy commands in parallel.** Run `cargo fmt`, `cargo clippy`, and `cargo test` one at a time. Before each, `pgrep -a cargo` must be empty. Do not dispatch parallel subagents that build or test.
- Local verification order: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- No new dependencies. Use the existing HSL/lerp helpers and WCAG maths.
- Default theme is `charcoal` + `sky`; app surfaces must be byte-identical to the current constants.
- Every UI colour goes through `Palette`; no `rgb(0x` / `rgba(0x` outside `palette.rs` except fully transparent `0x00000000`.
- Japanese UI labels, English code comments/docs.
- Commit prefixes: `feat:`, `fix:`, `docs:`, `chore:`.

## Review Focus

- **White accent on a light main:** accent text must stay readable while the swatch stays white; `accent`, `accent_soft`, and `accent_soft_hover` must all pass 4.5:1. Pinned by `all_pairs_are_readable`.
- **Light-mode inputs/textarea:** `colors.background` must stay opaque and only `tokens.background` cleared, or inputs become transparent. Pinned by the manual checklist in Task 6.
- **Pastel/light viewer overlays:** the stage, top bar, and year rail must not leave light text on a light surface. Pinned by Task 4d's file list and the manual checklist.
- **Legacy dark layers:** modal scrims, photo overlays, and notification surfaces must move to tokens or they break light mode. Pinned by the source guard in Task 5.
- **Invalid saved IDs:** an unknown ID must not reset the rest of the settings. Pinned by `unknown_theme_id_loads_verbatim` and `resolve_falls_back_to_defaults`.

---

### Task 1: Palette module (math, catalog, derivation)

**Files:**
- Create: `crates/ui/src/ui/palette.rs`
- Modify: `crates/ui/src/ui/mod.rs` (add `pub(crate) mod palette;`)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) is_dark: bool,
    pub(crate) bg: u32,
    pub(crate) glass: u32,
    pub(crate) panel: u32,
    pub(crate) panel_2: u32,
    pub(crate) panel_sidebar: u32,
    pub(crate) panel_header: u32,
    pub(crate) panel_card: u32,
    pub(crate) panel_inner: u32,
    pub(crate) panel_deep: u32,
    pub(crate) border: u32,
    pub(crate) border_strong: u32,
    pub(crate) line_soft: u32,
    pub(crate) highlight: u32,
    pub(crate) shadow: u32,
    pub(crate) scrim: u32,
    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) muted_2: u32,
    pub(crate) accent: u32,
    pub(crate) accent_fill: u32,
    pub(crate) accent_soft: u32,
    pub(crate) accent_soft_hover: u32,
    pub(crate) accent_border: u32,
    pub(crate) on_accent: u32,
    pub(crate) green: u32,
    pub(crate) green_soft: u32,
    pub(crate) amber: u32,
    pub(crate) amber_soft: u32,
    pub(crate) red: u32,
    pub(crate) red_soft: u32,
    pub(crate) red_border: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family { Dark, Light, Pastel }

#[derive(Clone, Copy)]
pub(crate) struct MainColor {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) family: Family,
    pub(crate) hue: f32,
    pub(crate) tint: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct AccentColor {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) family: Family,
    pub(crate) hue: f32,
    pub(crate) saturation: f32,
    pub(crate) lightness: f32,
}

pub(crate) fn main_colors() -> &'static [MainColor];
pub(crate) fn accent_colors() -> &'static [AccentColor];
pub(crate) fn resolve(main_id: &str, accent_id: &str) -> Palette;
pub(crate) fn preview_main(color: &MainColor) -> u32; // retinted panel_2
pub(crate) fn preview_accent(color: &AccentColor) -> u32; // raw base
pub(crate) fn apply_component_theme(palette: Palette, cx: &mut gpui_kit::App);
pub(crate) fn with_alpha(color: u32, alpha: u8) -> u32;
pub(crate) fn mix(base: u32, color: u32, weight: f32) -> u32;
pub(crate) fn contrast_ratio(a: u32, b: u32) -> f32;
pub(crate) fn ensure_contrast(fg: u32, bg: u32, min: f32) -> u32;
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32;
pub(crate) fn rgb_to_hsl(color: u32) -> (f32, f32, f32);

impl Palette {
    pub(crate) fn scene_color(&self, world: &str) -> u32;
}
```

- Consumes: nothing from later tasks.

- [ ] **Step 1: Write the failing tests in `palette.rs`**

Add a `#[cfg(test)] mod tests` with: `hsl_roundtrip`, `contrast_ratio_known_pairs`, `ensure_contrast_reaches_target`, `catalog_has_18_per_axis_and_6_per_family`, `resolve_falls_back_to_defaults`, `default_matches_current_palette`, `all_pairs_are_readable`, `scene_color_follows_the_family`. Assertions:
- `contrast_ratio(0x000000, 0xffffff)` is 21.0 ±0.01.
- `resolve("charcoal", "sky").panel_card == 0x0d1420`, `.panel_header == 0x101725`, `.panel_sidebar == 0x0b111b`, `.text == 0xeef3fb`, `.accent == 0x66b3ff`.
- `resolve("charcoal", "sky").accent_soft` is within 4 per RGB channel of `0x1a2f47`.
- All 324 pairs pass the contrast thresholds listed in the spec (`text` ≥7; `muted` ≥4.5; `muted_2` ≥3; accent vs the four surfaces ≥4.5; status vs its soft ≥4.5; `on_accent` vs `accent_fill` ≥4.5; `border` ≥1.2; `border_strong` ≥1.8).
- `scene_color` lightness: dark < 0.25, light/pastel > 0.75.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui palette::tests --no-default-features`
Expected: compile errors for the missing types/functions.

- [ ] **Step 3: Implement the types, catalog, and colour maths**

Use the catalog and template tables from the spec verbatim. Implement HSL↔RGB with sRGB values, WCAG relative luminance with the 0.03928 threshold, binary-search lightness adjustment that preserves hue/saturation, and the accent/status soft-fill loops from the spec. `resolve` falls back to `charcoal`/`sky`. `scene_color` hashes the world name to a hue offset and uses the family lightness (dark 0.15, light 0.90, pastel 0.84).

- [ ] **Step 4: Run the palette tests**

Run: `cargo test -p vrc-photo-ui palette::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/palette.rs crates/ui/src/ui/mod.rs
git commit -m "feat(ui): add the runtime color palette"
```

---

### Task 2: Persist the theme choice

**Files:**
- Modify: `crates/core/src/settings.rs`

**Interfaces:**
- Produces: `AppSettings.ui_theme_main: String` (default `"charcoal"`), `AppSettings.ui_theme_accent: String` (default `"sky"`).

- [ ] **Step 1: Write failing tests**

Add `theme_settings_roundtrip`, `missing_theme_fields_default_to_charcoal_and_sky`, `unknown_theme_id_loads_verbatim`. The first saves and reloads `AppSettings` with non-default IDs; the second parses the existing "settings without UI fields" JSON; the third parses an unknown string and asserts the rest of the settings survive.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core theme_settings`
Expected: compile errors for missing fields.

- [ ] **Step 3: Add the fields and defaults**

Add `#[serde(default = "default_ui_theme_main")]` and `default_ui_theme_accent`, set them in `Default`, and include them in the existing roundtrip fixtures if they enumerate fields.

- [ ] **Step 4: Run the core tests**

Run: `cargo test -p vrc-photo-core settings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs
git commit -m "feat(core): persist the selected color theme"
```

---

### Task 3: Give `App` a palette and apply the component theme

**Files:**
- Modify: `crates/ui/src/app.rs` (field, struct literal)
- Modify: `crates/ui/src/app/lifecycle.rs` (resync in `reload_aux`)
- Modify: `crates/ui/src/ui/theme.rs` (re-export `Palette`)
- Modify: `crates/ui/src/main.rs` (remove the forced dark block)

**Interfaces:**
- Consumes: `palette::resolve`, `palette::apply_component_theme`, `AppSettings.ui_theme_*`.
- Produces: `App.palette: Palette`.

- [ ] **Step 1: Add the field and initialise it**

In `App`, add `pub(crate) palette: Palette`. In `App::new`, resolve from `initial_settings` before the struct literal and insert it. After `app.reload_all(true, cx)`, call `palette::apply_component_theme(app.palette, cx)`.

- [ ] **Step 2: Resync after settings reloads**

At the end of `reload_aux`, resolve from the freshly loaded settings; if it differs from `self.palette`, assign and call `apply_component_theme`. This covers the "設定を再読込" button and external edits.

- [ ] **Step 3: Remove the forced dark theme**

In `main.rs`, delete the `Theme::change(..., Dark, ...)` block and its `global_mut` background override. `gpui_kit::init(cx)` stays. `apply_component_theme` now owns the transparent `tokens.background`.

- [ ] **Step 4: Check the crate**

Run: `cargo check -p vrc-photo-ui`
Expected: PASS (no screen uses the palette yet).

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/lifecycle.rs crates/ui/src/ui/theme.rs crates/ui/src/main.rs
git commit -m "feat(ui): apply the saved theme at startup"
```

---

### Task 4a: Move shared widgets and notifications to the palette

**Files:**
- Modify: `crates/ui/src/ui/components.rs`
- Modify: `crates/ui/src/ui/notifications.rs`
- Modify: `crates/ui/src/ui/settings.rs` (colour references only; the theme card comes in Task 6)
- Modify: call sites of `glass_panel`, `dialog_panel`, `chip`, `status_progress_bar`, `manual_option_row` in every screen

**Interfaces:**
- Consumes: `App.palette`.
- Produces: `glass_panel(&Palette)`, `dialog_panel(&Palette)`, `chip(&Palette, label, active)`, `status_progress_bar(&Palette, ratio)`, `manual_option_row(&Palette, ...)`.

- [ ] **Step 1: Change the helper signatures and bodies**

Add `palette: &Palette` as the first parameter. Add `let p = palette;` and replace every old constant (`PANEL_CARD`, `BORDER`, `TEXT`, `BLUE`, `ACCENT_SOFT`, `MUTED`, …) and every literal with palette fields. Use `with_alpha(p.accent, a)` for accent alphas and `p.line_soft` / `p.highlight` for the white overlays.

- [ ] **Step 2: Update call sites**

For `impl App` callers, pass `&self.palette`. For free functions, thread the palette through. Use the compiler errors to find every call site.

- [ ] **Step 3: Update `notifications.rs`**

It is an `impl App` render path, so use `let p = self.palette;` and map the bell badge (`AMBER` + its glow), modal scrim (`0x05070acc`), panel surfaces, borders, and text to palette fields.

- [ ] **Step 4: Check**

Run: `cargo check -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/components.rs crates/ui/src/ui/notifications.rs crates/ui/src/ui/settings.rs crates/ui/src
git commit -m "refactor(ui): route shared widgets through the palette"
```

---

### Task 4b: Shell, chrome, dashboard, app root

**Files:**
- Modify: `crates/ui/src/ui/shell.rs`
- Modify: `crates/ui/src/ui/chrome.rs`
- Modify: `crates/ui/src/ui/dashboard.rs`
- Modify: `crates/ui/src/app.rs` (render root `.bg(rgba(p.glass))`)

- [ ] **Step 1: Replace constants and literals**

In each `impl App` method add `let p = self.palette;`. Replace `BG`, `PANEL`, `PANEL_2`, `PANEL_SIDEBAR`, `PANEL_HEADER`, `PANEL_CARD`, `PANEL_INNER`, `PANEL_DEEP`, `BORDER`, `BORDER_STRONG`, `TEXT`, `MUTED`, `MUTED_2`, `BLUE`, `ACCENT_SOFT`, `GREEN`, `AMBER`, `RED`. Replace literals per the spec's table: white/black overlays → `line_soft`/`highlight`/`shadow`, accent alphas → `with_alpha(p.accent, a)`, neutral surfaces → the nearest panel token.

- [ ] **Step 2: Root glass**

`App::render` uses `rgba(self.palette.glass)`.

- [ ] **Step 3: Check**

Run: `cargo check -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/shell.rs crates/ui/src/ui/chrome.rs crates/ui/src/ui/dashboard.rs crates/ui/src/app.rs
git commit -m "refactor(ui): theme the shell and dashboard"
```

---

### Task 4c: Library, search, year rail, photo cards

**Files:**
- Modify: `crates/ui/src/ui/library.rs`
- Modify: `crates/ui/src/ui/search.rs`
- Modify: `crates/ui/src/ui/year_rail.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/state/year_rail.rs` (only if it carries colours)
- Modify: `crates/ui/src/ui/theme.rs` (remove `scene_color` after callers move)

- [ ] **Step 1: Replace constants and literals**

Same rules as Task 4b. In `app/photos.rs`, replace `BG` with `p.bg` and `scene_color(world)` with `p.scene_color(world)`. Move the hash helper into `Palette`.

- [ ] **Step 2: Year rail**

The marker becomes `p.panel_header` with `p.text` and a `p.border_strong` ring; the hover/selected rings use `p.line_soft`/`p.highlight`.

- [ ] **Step 3: Check and test**

Run: `cargo check -p vrc-photo-ui`
Run: `cargo test -p vrc-photo-ui state::year_rail`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/library.rs crates/ui/src/ui/search.rs crates/ui/src/ui/year_rail.rs crates/ui/src/app/photos.rs crates/ui/src/state/year_rail.rs crates/ui/src/ui/theme.rs
git commit -m "refactor(ui): theme the library and year rail"
```

---

### Task 4d: Viewer, import, compression, dialogs, update view

**Files:**
- Modify: `crates/ui/src/ui/viewer.rs`
- Modify: `crates/ui/src/ui/import.rs`
- Modify: `crates/ui/src/ui/compression.rs`
- Modify: `crates/ui/src/ui/dialogs.rs`
- Modify: `crates/ui/src/update_view.rs`

- [ ] **Step 1: Replace constants and literals**

Same rules. Additionally:
- viewer stage `rgb(0x05070a)` → `p.bg`; morph backdrop → `p.bg`.
- viewer top bar `rgba(0x05070a99)` → `with_alpha(p.panel_header, 0x99)`; its border `rgba(0xffffff12)` → `p.line_soft`.
- modal backdrops `rgba(0x05070acc)` → `p.scrim`.
- danger/amber literals → `p.red*`/`p.amber*`.
- `update_downloading_view` gains a `&Palette` parameter; its caller passes `&self.palette`.

- [ ] **Step 2: Check**

Run: `cargo check -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ui/src/ui/viewer.rs crates/ui/src/ui/import.rs crates/ui/src/ui/compression.rs crates/ui/src/ui/dialogs.rs crates/ui/src/update_view.rs
git commit -m "refactor(ui): theme the viewer and dialogs"
```

---

### Task 5: Remove the old constants and guard against regressions

**Files:**
- Modify: `crates/ui/src/ui/theme.rs`
- Modify: `crates/ui/src/ui/palette.rs` (guard test)

- [ ] **Step 1: Delete the colour constants**

Remove `BG`, `PANEL`, `PANEL_2`, `SIDEBAR`, `GLASS_WINDOW`, `PANEL_SIDEBAR`, `PANEL_SIDEBAR_DEEP`, `PANEL_HEADER`, `PANEL_CARD`, `PANEL_INNER`, `PANEL_INNER_DEEP`, `PANEL_DEEP`, `PANEL_RAISED`, `BORDER`, `BORDER_STRONG`, `TEXT`, `MUTED`, `MUTED_2`, `BLUE`, `ACCENT_SOFT`, `GREEN`, `AMBER`, `RED`. Keep layout constants and helpers. Re-export `Palette` from `palette`.
Update/remove `glass_and_panel_tokens_follow_the_spec` and any test importing the removed names; keep `lerp_color` (move it to `palette.rs` or re-export it).

- [ ] **Step 2: Add the source guard test**

`no_hardcoded_palette_colors_outside_palette` walks `crates/ui/src/**/*.rs`, skips `ui/palette.rs`, and fails on `rgb(0x` / `rgba(0x`, except `rgba(0x00000000)`. If a non-palette module must keep a literal for image data, add an explicit allowlist entry in the test with a comment.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p vrc-photo-ui palette::tests`
Run: `cargo test -p vrc-photo-ui no_hardcoded`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/ui/theme.rs crates/ui/src/ui/palette.rs
git commit -m "refactor(ui): remove the static palette and guard it"
```

---

### Task 6: Settings UI

**Files:**
- Modify: `crates/ui/src/ui/settings.rs`

**Interfaces:**
- Consumes: `main_colors()`, `accent_colors()`, `preview_main`, `preview_accent`, `App.palette`.
- Produces: `App::set_main_theme(&mut self, id: &str, cx)`, `App::set_accent_theme(&mut self, id: &str, cx)`.

- [ ] **Step 1: Add the theme card**

At the top of `settings_screen`, before the library card, add a `w_full()` `glass_panel(&self.palette)` with the title `テーマ`, a `メインカラー（背景・パネル）` block and a `サブカラー（アクセント）` block. Each block is rows of six 30 px circles (`preview_main`/`preview_accent`) with no family headings and no inline names; `w_full()` + `justify_between()` spans the card. Each circle has an `.id(SharedString)`, a tooltip with the colour name, a selected ring (`p.accent_border`), and a check mark using `readable_on(swatch)`. End with the note: `サブカラーは背景とのコントラストに合わせて明度を自動調整します`.

- [ ] **Step 2: Add the setters**

```rust
pub(crate) fn set_main_theme(&mut self, id: &str, cx: &mut Context<Self>);
pub(crate) fn set_accent_theme(&mut self, id: &str, cx: &mut Context<Self>);
```

Each returns early if unchanged; otherwise capture the previous settings + palette, resolve, assign, save, and on error restore both and report through `self.message`/toast. On success call `apply_component_theme` and `cx.notify()`.

- [ ] **Step 3: Check and test**

Run: `cargo check -p vrc-photo-ui`
Run: `cargo test -p vrc-photo-core settings`
Expected: PASS.

- [ ] **Step 4: Manual check (demo mode)**

Run the app with `--demo`, open Settings, click through `charcoal` + `sky`, `paper` + `sky`, `sakura` + `white`, and `paper` + `white`. Confirm: swatches are distinguishable, text stays readable, inputs have a visible border/fill, primary buttons follow the accent, restart keeps the selection. Record the four screenshots in the PR.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/settings.rs
git commit -m "feat(ui): add the color theme picker"
```

---

### Task 7: Full verification

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Expected: no remaining diff from `cargo fmt --all -- --check`.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 3: Tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit any fixes**

Use `fix:` or `chore:` as appropriate.

---

### Task 8: Push and open the combined PR

- [ ] **Step 1: Push**

```bash
git push -u origin feat/color-themes
```

- [ ] **Step 2: Open one PR against `main`**

Title: `feat: notification center and selectable color themes`
Body sections: notifications (from the base branch), colour themes (18 × 18, default unchanged, viewer follows the theme), verification commands, screenshots, and the manual checklist. Note that the branch is stacked on `feat/ui-notifications` and the PR contains both.

- [ ] **Step 3: Monitor CI sequentially**

Follow the `ci-watch` skill. Resolve the latest run on every poll; do not run local cargo while CI is active. On red, analyze and push a fix; on green, report the PR URL and check status to the user. Do not publish a release or tag.

---

## Self-review notes

- Spec coverage: palette model/catalog/derivation (Task 1), persistence (Task 2), application/component theme (Tasks 3, 4a-d), literal removal (Tasks 4a-d, 5), settings UI (Task 6), viewer (Task 4d), verification/PR (Tasks 7-8).
- Type consistency: `Palette` fields and helper signatures are defined once in Task 1 and referenced by name in later tasks.
- The plan keeps the tree compiling after every task; the old constants stay until Task 5.
- Heavy commands are sequential by global constraint.
