# UI Viewer and Shell Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the nine UI follow-ups from issue #34 in the GPUI app, including a fixed shared header across Photos, Compression, Monitor, and Settings.

**Architecture:** Keep `crates/ui/src/main.rs` as the single UI file. First fix viewer state, button tones, boundary navigation, image-box sizing, stale-selection handling, and focus races. Then stabilize the grid's intro and scroll behavior. Finally unify the three page-header layouts and the chip styles. Pure geometry gets unit tests; GPUI behavior is verified with the WSL capture workflow.

**Tech Stack:** Rust 2024, gpui-kit 0.8.0 / gpui-pre 0.3.9, `uniform_list`, SQLite demo fixtures.

**Spec:** GitHub issue [#34](https://github.com/TommyZ-7/vrc-photo-manager/issues/34) plus the Decisions section below.

## Decisions

| Area | Decision |
|---|---|
| Delivery | One code PR with three commits (viewer / grid / header). This plan document is a separate docs-only PR, per AGENTS.md §4. |
| Header unification | Photos, Compression, Monitor, and Settings use the same fixed 72 px header. Search keeps its two-column in-body layout. Photos keeps its static filter-chip row directly below the fixed header. |
| Scroll preservation | Keep the `uniform_list` element ID stable so the existing pixel offset survives column-count changes. Do not add anchor-photo tracking. |
| Viewer status message | Clear `App::message` when the viewer opens. A status from the previous trashing/compression session is never useful inside a freshly opened viewer. |
| Disabled boundary buttons | Do not attach `cursor_pointer`, hover, focus, tab-stop, click, or key handlers. Render muted. Keep arrow-key navigation clamping as-is. |
| Viewer image box | Compute width/height from `window.viewport_size()`, the top bar, padding, the resolved details width, and the photo aspect. The image area clips during the 300 ms details-width animation. |
| Missing selection | If `selected_photo` becomes `None` while the viewer is open, close it on the next render, clear the selection, and restore focus. |

## Global Constraints

- Modify only `crates/ui/src/main.rs` (and tests inside that file). Do not split the UI into modules.
- Do not add dependencies or bump `gpui-kit = "=0.8.0"`.
- Do not touch `vrc-photo-core`, the daemon, the DB schema, or `AppSettings` keys.
- Preserve import, search, compression, monitor, settings, tag, favorite, and trash behavior except for the nine items listed here.
- Every `Animation::new(...)` keeps the existing easing/max-FPS rules from the UI-redesign plan.
- Disabled viewer buttons must not be reachable with Tab and must not react to hover.
- Screenshots and temporary files stay in `/tmp/opencode`; never commit them.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Do not create tags, run the Release workflow manually, or publish a release.
- Use a worktree for the code branch. Do not push to `main`.

## Review Focus

1. Trash a photo, let the viewer close, then reopen the viewer: no stale “写真をゴミ箱へ移動しました” line appears in the details panel, and the trash action is not still armed.
2. At the first photo, “前へ” is muted, has no pointing cursor, no hover response, and no Tab stop; at the last photo, “次へ” behaves the same. Arrow keys still stop at the ends. Tab from “閉じる” goes to the next enabled control, and the details toggle keeps focus after opening/closing the panel.
3. At 1280×820 and 1440×900, clicking the dark area above/below/left/right of the image closes the viewer, including when the details panel is open. Clicking the image itself never closes it. Portrait and landscape demo photos both fit inside the padded area.
4. Delete the selected photo row while the viewer is open: within one 3-second refresh the viewer disappears, the library selection clears, and focus returns to the grid. No invisible overlay blocks shortcuts.
5. Open the viewer and press Esc immediately, repeatedly: the deferred close-focus cannot steal focus after the viewer is gone. Tab after closing goes to a mounted control.
6. Hover cards during the first 600 ms after launch, and hover while scrolling: the intro fade/slide plays once per photo and does not replay. Scrolling down and toggling the sidebar (`Ctrl+B`) or inspector (`Ctrl+I`) keeps the grid's scroll offset instead of jumping to the top.
7. At 1280×820 and 1440×900, Photos, Compression, Monitor, and Settings all show the same fixed 72 px header with a bottom border. Compression's body scrolls under the header; Photos' chip row stays directly below the header. No header actions overflow the window.

---

## Task 1: Viewer state and controls (issue items 1–6)

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields ~150–208; `open_quick_look` ~578; `close_quick_look` ~605; `quick_look_focus_order` ~663; `toggle_quick_look_details` ~654; `trash_selected` ~701; `focusable_text_button` ~2013; `detail_body` ~2084; `quick_look` ~2308; `Render::render` ~3733; tests ~4391)

**Interfaces:**
- Produces: `enum ButtonTone { Neutral, Primary, Danger }` (module level, near `PhotoCardLayout`).
- Produces: `fn focusable_text_button(&self, id: &'static str, label: impl Into<SharedString>, focus: FocusHandle, tone: ButtonTone, enabled: bool, cx: &mut Context<Self>, on_activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static) -> AnyElement`.
- Produces: `const VIEWER_TOP_BAR: f32 = 56.0;` and `const VIEWER_PAD: f32 = 24.0;`.
- Produces: `fn viewer_image_box(viewport_w: f32, viewport_h: f32, details_open: bool, aspect: f32) -> (f32, f32)`.
- Produces: `fn guard_viewer_selection(&mut self, window: &mut Window, cx: &mut Context<Self>)`.

- [ ] **Step 1: Write the failing geometry test**

In `mod tests`, import `viewer_image_box` and add:

```rust
#[test]
fn viewer_image_box_fits_inside_padding_and_details() {
    let close = |a: f32, b: f32| assert!((a - b).abs() < 0.01, "{a} != {b}");

    let (w, h) = viewer_image_box(1280.0, 820.0, false, 16.0 / 9.0);
    close(w, 1232.0);
    close(h, 693.0);

    let (w, h) = viewer_image_box(1280.0, 820.0, true, 16.0 / 9.0);
    close(w, 852.0);
    close(h, 479.25);

    let (w, h) = viewer_image_box(1280.0, 820.0, false, 2.0 / 3.0);
    close(w, 477.333);
    close(h, 716.0);

    let (w, h) = viewer_image_box(400.0, 400.0, false, 1.0);
    close(w, 296.0);
    close(h, 296.0);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-ui viewer_image_box_fits_inside_padding_and_details`
Expected: FAIL because `viewer_image_box` is not defined.

- [ ] **Step 3: Add the constants and implement `viewer_image_box`**

Near `VIEWER_DETAILS_WIDTH`:

```rust
const VIEWER_TOP_BAR: f32 = 56.0;
const VIEWER_PAD: f32 = 24.0;
```

Add near the other pure helpers:

```rust
fn viewer_image_box(
    viewport_w: f32,
    viewport_h: f32,
    details_open: bool,
    aspect: f32,
) -> (f32, f32) {
    let details = if details_open { VIEWER_DETAILS_WIDTH } else { 0.0 };
    let avail_w = (viewport_w - details - VIEWER_PAD * 2.0).max(1.0);
    let avail_h = (viewport_h - VIEWER_TOP_BAR - VIEWER_PAD * 2.0).max(1.0);
    let ratio = aspect.max(0.05);
    let width = avail_w.min(avail_h * ratio);
    (width, width / ratio)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vrc-photo-ui viewer_image_box_fits_inside_padding_and_details`
Expected: PASS.

- [ ] **Step 5: Add `ButtonTone` and change `focusable_text_button`**

Add the enum:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonTone {
    Neutral,
    Primary,
    Danger,
}
```

Change the signature to take `tone: ButtonTone` and `enabled: bool` in place of `primary: bool`.

Build the base `div` with the tone palette:

- `Primary`: border `rgba(0x66b3ff40)`, bg `rgba(0x66b3ff1a)`, text `rgb(0xdceeff)`.
- `Neutral`: border `rgba(0xffffff10)`, bg `rgba(0xffffff08)`, text `rgb(MUTED_2)`.
- `Danger`: border `rgba(0xfb718566)`, bg `rgba(0xfb718526)`, text `rgb(RED)`.
- Disabled (any tone): border `rgba(0xffffff08)`, bg `rgba(0x00000000)`, text `rgba(0x76819680)`.

Only when `enabled` is true, attach:

```rust
.cursor_pointer()
.track_focus(&focus)
.tab_stop(true)
.hover(...)
.focus(...)
.on_click(...)
.on_key_down(...)
```

Hover/focus colors follow the tone: `Primary` uses the existing blue hover/focus values; `Neutral` keeps the existing white-alpha hover. For `Danger`, hover uses bg `rgba(0xfb71853d)` and text `rgb(0xffd9df)`, and focus uses border `rgb(RED)`, bg `rgba(0xfb718533)`, and text `rgb(0xffd9df)`.

When `enabled` is false, attach no cursor, focus, tab-stop, hover, click, or key handlers.

- [ ] **Step 6: Update all `focusable_text_button` call sites**

Use these tones:

| Call site | Tone | Enabled |
|---|---|---|
| `viewer-favorite` | `Primary` when favorite, else `Neutral` | always true |
| `viewer-add-tag` | `Neutral` | always true |
| `viewer-compress` | `Primary` | always true |
| `viewer-trash` | `Danger` when armed, else `Neutral` | always true |
| `viewer-close` | `Primary` | always true |
| `viewer-prev` | `Primary` when `prev_enabled`, else `Neutral` | `prev_enabled` |
| `viewer-next` | `Primary` when `next_enabled`, else `Neutral` | `next_enabled` |
| `viewer-details` | `Primary` when details open, else `Neutral` | always true |

- [ ] **Step 7: Skip disabled boundary buttons in the focus order**

In `quick_look_focus_order`, clamp the index the same way `quick_look` does, then build the base order conditionally:

```rust
let index = self
    .quick_look_index
    .min(self.quick_look_photo_ids.len().saturating_sub(1));
let mut order = vec![self.quick_look_close_focus.clone()];
if index > 0 {
    order.push(self.quick_look_prev_focus.clone());
}
if index + 1 < self.quick_look_photo_ids.len() {
    order.push(self.quick_look_next_focus.clone());
}
order.push(self.quick_look_details_focus.clone());
```

Keep the details-panel controls appended when `quick_look_details_open` is true.

In `toggle_quick_look_details`, replace the hard-coded `3` with the actual position of `self.quick_look_details_focus` in `self.quick_look_focus_order(cx)`, falling back to `0` if it is absent. Focus that index after updating the state.

In `step_viewer_photo`, after updating `self.quick_look_index`, check whether the newly disabled boundary button currently has focus. If it does, move focus to `self.quick_look_close_focus` and set `self.quick_look_focus_index = 0`. This prevents focus from being stranded on a button that just lost its tab stop.

- [ ] **Step 8: Clear the stale status message and guard the deferred focus**

At the start of `open_quick_look` (after the early return), add:

```rust
self.message = None;
```

Replace the existing `window.defer` retry with `cx.defer_in`:

```rust
let epoch = self.quick_look_epoch;
let close_focus = self.quick_look_close_focus.clone();
cx.defer_in(window, move |this, window, cx| {
    if this.quick_look_open && this.quick_look_epoch == epoch {
        window.focus(&close_focus, cx);
    }
});
```

Keep the immediate `window.focus(&close_focus, cx)` before the deferred retry.

- [ ] **Step 9: Add and call `guard_viewer_selection`**

Implement:

```rust
fn guard_viewer_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if !self.quick_look_open || self.selected_photo.is_some() {
        return;
    }
    self.quick_look_open = false;
    self.delete_armed = None;
    self.selected = None;
    self.quick_look_photo_ids.clear();
    self.quick_look_index = 0;
    self.preview = None;
    self.previous_preview = None;
    self.previous_color = None;
    let focus = self
        .quick_look_return_focus
        .take()
        .unwrap_or_else(|| self.photo_grid_focus.clone());
    cx.defer_in(window, move |this, window, cx| {
        if !this.quick_look_open {
            window.focus(&focus, cx);
        }
    });
}
```

Call it as the first statement in `Render::render`, before reading `window.viewport_size()`.

- [ ] **Step 10: Compute the image box from the viewport**

Change the signature to `fn quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement` and update the caller to `self.quick_look(window, cx)`.

Inside, after computing `aspect`, compute:

```rust
let viewport = window.viewport_size();
let (image_w, image_h) = viewer_image_box(
    viewport.width.into(),
    viewport.height.into(),
    details_open,
    aspect,
);
```

Replace the image element's `.h_full().aspect_ratio(aspect).max_w(relative(0.92)).max_h(relative(0.92))` with `.w(px(image_w)).h(px(image_h))` in both the preview and fallback branches.

Use `VIEWER_TOP_BAR` for the top bar height and `VIEWER_PAD` for the image-area padding. Add `.overflow_hidden()` to `image_area` so the image is clipped during the 300 ms details-width animation.

Keep `.on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))` on the image so only the surrounding dark area closes the viewer.

- [ ] **Step 11: Run formatting, clippy, and tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all green.

- [ ] **Step 12: Smoke-check the viewer**

Launch `cargo run -p vrc-photo-ui -- --demo` and check:

- Trash the selected photo (two clicks), reopen the viewer on another photo: no stale message.
- First photo: “前へ” is muted and does not focus with Tab; last photo: “次へ” is muted.
- Click the dark area above/below the image (details open and closed): the viewer closes.
- Click the image: it stays open.

- [ ] **Step 13: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "fix: harden viewer state and polish its controls"
```

---

## Task 2: Grid intro and scroll stability (issue items 7–8)

**Files:**
- Modify: `crates/ui/src/main.rs` (`photo_list` ~1358; `photo_card` ~1444–1639)

**Interfaces:**
- Consumes: the existing `CARD_INTRO_SECS`, `CARD_STAGGER_SECS`, `CARD_STAGGER_MAX`, `ease_motion`, and `stagger_value`.
- Produces: no new public interface. The intro animation becomes the outer wrapper; the hover animation becomes an inner wrapper; the `uniform_list` ID becomes the constant string `"photo-grid-list"`.

- [ ] **Step 1: Move the intro animation outside the hover animation**

Restructure `photo_card` as follows:

1. Leave `body` as a plain `Div`; delete the `if intro { body.with_animation(...) } else { ... }` block and the `let body: AnyElement` conversion.
2. Build `card` exactly as today, but wrap it with the hover animation:

```rust
let card = v_flex()
    .id(SharedString::from(format!("photo-{id}")))
    .relative()
    .w_full()
    .cursor_pointer()
    .on_hover(...)
    .on_click(...)
    .child(body)
    .with_animation(
        SharedString::from(format!("card-hover-{id}-{hovered}")),
        Animation::new(Duration::from_millis(180)).with_easing(ease_motion),
        move |el, t| {
            let offset = if hovered { -4.0 * t } else { -4.0 * (1.0 - t) };
            el.relative().top(px(offset))
        },
    );
```

3. Put the grid sizing on an outer cell and attach the intro there:

```rust
let cell = div()
    .relative()
    .when(grid_index.is_some(), |this| this.flex_1().min_w_0())
    .when(grid_index.is_none(), |this| this.w(px(190.)))
    .child(card);

if intro {
    cell.with_animation(
        SharedString::from(format!("card-intro-{grid_epoch}-{id}")),
        Animation::new(Duration::from_secs_f32(intro_total))
            .with_max_fps(60.0),
        move |el, t| {
            let value = stagger_value(t, intro_index);
            el.opacity(value).relative().top(px(14.0 * (1.0 - value)))
        },
    )
    .into_any_element()
} else {
    cell.into_any_element()
}
```

The key point: the intro `GlobalElementId` path must stay stable while `hovered` changes, so the intro state is not remounted.

- [ ] **Step 2: Stabilize the `uniform_list` ID**

Replace:

```rust
SharedString::from(format!("photo-grid-{columns}"))
```

with:

```rust
SharedString::from("photo-grid-list")
```

Add a comment above it:

```rust
// Keep the list ID stable so uniform_list retains its scroll offset when
// the column count changes; row height is fixed.
```

- [ ] **Step 3: Run formatting, clippy, and tests**

Run the three commands from Task 1 Step 11. Expected: all green.

- [ ] **Step 4: Smoke-check motion and scroll**

Launch `--demo` and check:

- Hover several cards during the first 600 ms: each card's intro plays once; hovering does not restart it.
- Scroll down, press `Ctrl+B` and `Ctrl+I`: the grid stays at the same scroll offset instead of jumping to the top.
- Resize the window across a column boundary (or use `--window-size 1280x820` / `1440x900`): the offset survives, and rows do not overflow their cells.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "fix: keep card intros and grid scroll stable"
```

---

## Task 3: Fixed shared headers and chip dedupe (issue item 9)

**Files:**
- Modify: `crates/ui/src/main.rs` (`screen_header` ~1160; `photos_header` ~1238; `compression_screen` ~2510; `preset_button` ~3233; `chip` ~4062)

**Interfaces:**
- Consumes: `screen_header(title: &'static str, subtitle: String, message: Option<String>, actions: Vec<AnyElement>) -> AnyElement`.
- Produces: `photos_header` and `compression_screen` call `screen_header`; no new public interface.
- Produces: `preset_button` built on top of `chip(label: &str, active: bool) -> Div`.

- [ ] **Step 1: Make Photos use `screen_header`**

In `photos_header`, delete the duplicated 72 px `h_flex` block (title block, message slot, inspector/import buttons). Replace it with:

```rust
let actions = vec![
    Button::new("inspector-toggle")
        .label(if inspector_collapsed { "詳細" } else { "詳細を隠す" })
        .small()
        .ghost()
        .tooltip("詳細パネル (Ctrl+I)")
        .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx)))
        .into_any_element(),
    Button::new("import-copy")
        .label("コピー取込")
        .small()
        .ghost()
        .on_click(cx.listener(|this, _, _, cx| this.import_dialog(ImportMode::Copy, cx)))
        .into_any_element(),
    Button::new("import-move")
        .label("移動取込")
        .small()
        .primary()
        .on_click(cx.listener(|this, _, _, cx| this.import_dialog(ImportMode::Move, cx)))
        .into_any_element(),
];

v_flex()
    .flex_shrink_0()
    .child(self.screen_header("ライブラリ", subtitle, message, actions))
    .child(
        h_flex()
            .px(px(26.))
            .pt(px(12.))
            .pb(px(14.))
            .gap(px(8.))
            .items_center()
            .child(chip("ワールド", true))
            .child(chip("プレイヤー", false))
            .child(chip("日時", false))
            .child(chip("タグ", false))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("並び: 新しい順"),
            ),
    )
    .into_any_element()
```

Keep the same labels, subtitle, and actions.

- [ ] **Step 2: Make Compression use `screen_header` and a fixed header**

Restructure `compression_screen` so the header is outside the scroll container:

```rust
let header = self.screen_header(
    "圧縮設定",
    "自動圧縮は元画像を保持します。置換は手動実行時のみです。".to_string(),
    message,
    vec![
        h_flex()
            .gap_1()
            .child(self.policy_button("元画像を保持", "keep", cx))
            .child(self.policy_button("置換（ゴミ箱）", "replace", cx))
            .into_any_element(),
        Button::new("add-profile")
            .label("＋ プロファイル")
            .small()
            .on_click(cx.listener(|this, _, _, cx| {
                let profile = CompressionProfile {
                    name: format!("新規プロファイル {}", this.profiles.len() + 1),
                    auto_enabled: false,
                    ..CompressionProfile::default()
                };
                let _ = this.db.save_profile(&profile);
                this.profiles = this.db.list_profiles().unwrap_or_default();
                this.profile_index = this
                    .profiles
                    .iter()
                    .position(|item| item.id == profile.id)
                    .unwrap_or(0);
                cx.notify();
            }))
            .into_any_element(),
        Button::new("manual-compress")
            .label("手動圧縮")
            .small()
            .primary()
            .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx)))
            .into_any_element(),
    ],
);

v_flex()
    .flex_1()
    .min_h_0()
    .child(header)
    .child(
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px(px(26.))
            .pt(px(22.))
            .pb(px(26.))
            .gap(px(24.))
            .child(/* the existing profile list + editor, unchanged */)
            .child(/* the existing stats + queue, unchanged */),
    )
```

Move the existing body children (the profile list/editor and stats/queue currently at ~2586–2680) into the inner scroll container without changing their styles or handlers. Do not leave a second title block inside the scroll area.

- [ ] **Step 3: Build `preset_button` on `chip`**

Replace the duplicated chip styling in `preset_button` with:

```rust
fn preset_button(
    &self,
    label: &str,
    days: Option<i64>,
    cx: &mut Context<Self>,
) -> impl IntoElement {
    let active = self.search_days == days;
    chip(label, active)
        .id(SharedString::from(format!("preset-{label}")))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.search_days = days;
            this.run_search(cx);
        }))
}
```

Do not change `chip`'s visual values or the Photos filter chips (they remain non-interactive).

- [ ] **Step 4: Verify the four fixed headers and Search**

Run the three verification commands from Task 1 Step 11.

Then launch `--demo` at 1280×820 and 1440×900 and check:

- Photos: fixed header + chip row; grid starts below the chips.
- Compression: fixed header stays in place while the profile editor and queue scroll.
- Monitor and Settings: unchanged shared header.
- Search: still the two-column in-body layout; preset chips still work.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "fix: unify page headers and chip styles"
```

---

## Task 4: Full verification, visual QA, and PR

**Files:**
- No code changes unless a check fails.
- Screenshots: `/tmp/opencode/ui-polish-*.png` (never committed).

- [ ] **Step 1: Run the full local verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all green.

- [ ] **Step 2: Run the WSL visual QA**

Use the `wsl-gpui-testing` skill. Use the dzn path for hover/click/scroll checks, and normal mode is acceptable for static layout checks. Build before launching:

```bash
cargo build -p vrc-photo-ui
```

For dzn, the patched `gpui-pre-wgpu` is required. Apply the skill's throwaway-worktree patch to the code worktree, build with the shared target directory, and then remove `vendor/` and run `git restore Cargo.toml Cargo.lock` before committing or pushing. Never commit the vendor copy or the `[patch.crates-io]` entry. If the dzn binary is unavailable, use normal (llvmpipe) mode for layout and click checks, then repeat the hover/scroll checks when dzn is available.

Capture at 1280×820 and 1440×900:

```bash
.opencode/skills/wsl-gpui-testing/scripts/launch.sh --dzn --demo --window-size 1280x820
.opencode/skills/wsl-gpui-testing/scripts/capture-window.sh /tmp/opencode/ui-polish-1280-photos.png
```

Repeat for 1440×900, then exercise the Review Focus checklist:

1. Trash → close → reopen: no stale message, no armed trash.
2. First/last photo boundary buttons: muted, no pointer, no hover, no Tab stop; arrow keys clamp; Tab order and details focus are coherent.
3. Letterbox click closes; image click does not; repeat with details open and with portrait/landscape photos.
4. Delete the selected photo row from the demo DB while the viewer is open (`/tmp/vrc-photo-manager-demo/library.db`), wait one refresh: the viewer closes, selection clears, focus returns.
5. Open + immediate Esc repeatedly: focus ends on the grid; Tab reaches a mounted control.
6. Hover during intro and after scrolling: no intro replay. Toggle `Ctrl+B` / `Ctrl+I` after scrolling: offset is preserved.
7. All four fixed headers at both sizes; Compression scrolls under its header; Photos chips stay below the header; Search still works.

Record PASS/FAIL for each check in the PR body. If any check fails, fix it, rerun Steps 1–2, and amend the owning task's commit (or add a `fix:` commit).

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin fix/ui-polish-followups
gh pr create --base main --head fix/ui-polish-followups \
  --title "fix(ui): close viewer and shell polish follow-ups" \
  --body-file /tmp/opencode/ui-polish-pr.md
```

The PR body must list the nine issue items, the three commits, the verification commands, the visual QA PASS/FAIL table, and `Closes #34`.

- [ ] **Step 4: Monitor CI and merge**

Use the `ci-watch` skill against `fix/ui-polish-followups`. Do not merge until `CI` and the PR-time `Release` bundle are green. After merge, switch the main worktree to `main`, pull, delete the local and remote branch, and remove the code worktree.

- [ ] **Step 5: Commit this plan before the code PR**

Commit this file on `docs/ui-polish-followups-plan` and open a docs-only PR before starting the code worktree:

```bash
git add docs/superpowers/plans/2026-10-22-ui-polish-followups.md
git commit -m "docs: add UI polish follow-up plan"
git push -u origin docs/ui-polish-followups-plan
```

The docs PR relies on the CI changes-job fast-pass; it must not contain code changes. After it merges, cut the code worktree from the updated `main`.
