# Motion and View-Transition Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make sidebar navigation, screen changes, the photo viewer, and secondary panels animate with the continuity described in the approved motion design.

**Architecture:** Keep the existing single-file GPUI app. Add pure geometry/timeline helpers next to the current motion helpers, capture window-space image bounds during prepaint, and render one temporary absolute overlay that morphs the cached thumbnail/preview between the card and the viewer rectangle. Sidebar and screen motion use the same `with_animation` / `ease_motion` primitives already in the file.

**Tech Stack:** Rust stable (edition 2024), gpui-kit 0.8 (GPUI Base animations), SQLite-backed UI state, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-25-motion-transitions-design.md`

## Global Constraints

- Everything lives in `crates/ui/src/main.rs` unless a step names another file. Do not split the UI into new modules in this plan.
- No new dependencies; gpui-kit stays pinned at `=0.8.0`.
- Use the existing `ease_motion` curve for every new animation.
- Animate only opacity, width, height, and small left/top offsets. Do not add `scale` or `rotate`.
- Add `.with_max_fps(60.0)` to animations that run longer than a few frames.
- All `with_animation` calls automatically honor `ui_reduce_motion`. Any custom timer (the viewer morph cleanup) must check `cx.reduce_motion()` before starting.
- Do not change focus order, keyboard shortcuts, hit-testing semantics, database schema, or settings keys.
- Preserve the current card/detail rounded-corner pattern: every image layer that needs rounded clipping carries its own `rounded(...)`.
- Verify with `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` before the final commit.
- Visual checks use `cargo run -p vrc-photo-ui -- --demo` (or the existing local demo launch command) on GNOME Wayland.

## Review Focus

- Opening the viewer and pressing Esc/next/details within the first ~340 ms: no stale previous image, no flash to full-size before closing.
- Closing the viewer when the source card is scrolled out of view: a graceful fade, never a flight to a stale rectangle.
- Stepping between portrait and landscape photos: the frame resizes without clipping or a one-frame snap, and the details panel stays stable.
- Reduce motion enabled: no morph overlay or timer runs; every transition is the immediate final state.
- Sidebar collapse during a tab change, or rapid tab switching: the pill and bar stay attached and do not jump to a stale position.

---

### Task 1: Motion Geometry Helpers

**Files:**
- Modify: `crates/ui/src/main.rs` (constants near the existing `const` block; helpers near `ease_motion()` / `viewer_image_box()`; tests in the existing `#[cfg(test)] mod tests`).

**Interfaces:**
- Consumes: existing `Screen`, `Bounds`, `Pixels`, `Size`, `point`, `size`, `px`, `ease_motion`, `viewer_image_box`, `VIEWER_DETAILS_WIDTH`, `VIEWER_TOP_BAR`.
- Produces:
  - `const MOTION_FAST: Duration` = 160 ms
  - `const MOTION_BASE: Duration` = 220 ms
  - `const MOTION_SLOW: Duration` = 300 ms
  - `const MOTION_MORPH: Duration` = 340 ms
  - `const MOTION_FPS: f32` = 60.0
  - `const SCREEN_SLIDE: f32` = 14.0
  - `const VIEWER_NAV_SLIDE: f32` = 28.0
  - `const NAV_ITEM_HEIGHT: f32` = 40.0
  - `const NAV_ITEM_GAP: f32` = 3.0
  - `fn nav_index(screen: Screen) -> usize`
  - `fn nav_item_top(index: usize) -> f32`
  - `fn nav_bar_left(collapsed: bool) -> f32`
  - `fn screen_direction(from: Screen, to: Screen) -> f32`
  - `fn lerp_bounds(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels>`
  - `fn eased_progress(started_at: Instant, duration: Duration, now: Instant) -> f32`
  - `fn morph_bounds_at(from: Bounds<Pixels>, to: Bounds<Pixels>, started_at: Instant, now: Instant) -> Bounds<Pixels>`
  - `fn viewer_image_rect(viewport: Size<Pixels>, details_open: bool, aspect: f32) -> Bounds<Pixels>`
  - `fn current_tagged_bounds(entry: Option<(u64, Bounds<Pixels>)>, current_epoch: u64) -> Option<Bounds<Pixels>>`
  - `fn preferred_bounds(prefer_card: bool, card: Option<Bounds<Pixels>>, inspector: Option<Bounds<Pixels>>) -> Option<Bounds<Pixels>>`

- [ ] **Step 1: Write the failing tests**

Add these tests next to `ease_motion_is_monotonic_and_clamped` in the existing tests module:

```rust
#[test]
fn nav_index_matches_sidebar_order() {
    assert_eq!(nav_index(Screen::Photos), 0);
    assert_eq!(nav_index(Screen::Search), 1);
    assert_eq!(nav_index(Screen::Compression), 2);
    assert_eq!(nav_index(Screen::Dashboard), 3);
    assert_eq!(nav_index(Screen::Settings), 4);
}

#[test]
fn nav_item_top_uses_item_height_and_gap() {
    assert_eq!(nav_item_top(0), 0.0);
    assert_eq!(nav_item_top(1), NAV_ITEM_HEIGHT + NAV_ITEM_GAP);
    assert_eq!(nav_item_top(4), 4.0 * (NAV_ITEM_HEIGHT + NAV_ITEM_GAP));
}

#[test]
fn screen_direction_follows_sidebar_order() {
    assert_eq!(screen_direction(Screen::Photos, Screen::Settings), 1.0);
    assert_eq!(screen_direction(Screen::Settings, Screen::Photos), -1.0);
    assert_eq!(screen_direction(Screen::Search, Screen::Search), 1.0);
}

#[test]
fn lerp_bounds_hits_endpoints_and_midpoint() {
    let from = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));
    let to = Bounds::new(point(px(30.), px(60.)), size(px(200.), px(150.)));
    assert_eq!(lerp_bounds(from, to, 0.0), from);
    assert_eq!(lerp_bounds(from, to, 1.0), to);
    let middle = lerp_bounds(from, to, 0.5);
    assert_eq!(f32::from(middle.origin.x), 20.0);
    assert_eq!(f32::from(middle.origin.y), 40.0);
    assert_eq!(f32::from(middle.size.width), 150.0);
    assert_eq!(f32::from(middle.size.height), 100.0);
}

#[test]
fn viewer_image_rect_centers_the_box_and_respects_details() {
    let viewport = size(px(1200.), px(800.));
    let closed = viewer_image_rect(viewport, false, 16.0 / 10.0);
    let open = viewer_image_rect(viewport, true, 16.0 / 10.0);
    assert!((f32::from(closed.center().x) - 600.0).abs() < 0.01);
    assert!((f32::from(closed.center().y) - 428.0).abs() < 0.01);
    assert!((f32::from(open.center().x) - 410.0).abs() < 0.01);
    assert!((f32::from(open.center().y) - 428.0).abs() < 0.01);
    assert!(closed.size.width > open.size.width);
}

#[test]
fn eased_progress_is_clamped_and_eased() {
    let start = Instant::now();
    assert_eq!(eased_progress(start, MOTION_BASE, start), 0.0);
    assert_eq!(
        eased_progress(start, MOTION_BASE, start + MOTION_BASE),
        1.0
    );
    let half = eased_progress(start, MOTION_BASE, start + MOTION_BASE / 2);
    assert!(half > 0.5 && half < 1.0);
    assert_eq!(eased_progress(start, Duration::ZERO, start), 1.0);
}

#[test]
fn morph_bounds_at_hits_both_ends() {
    let start = Instant::now();
    let from = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
    let to = Bounds::new(point(px(100.), px(200.)), size(px(300.), px(400.)));
    assert_eq!(morph_bounds_at(from, to, start, start), from);
    assert_eq!(
        morph_bounds_at(from, to, start, start + MOTION_MORPH),
        to
    );
}

#[test]
fn current_tagged_bounds_rejects_stale_frames() {
    let bounds = Bounds::new(point(px(1.), px(2.)), size(px(3.), px(4.)));
    assert_eq!(current_tagged_bounds(Some((7, bounds)), 7), Some(bounds));
    assert_eq!(current_tagged_bounds(Some((7, bounds)), 8), None);
    assert_eq!(current_tagged_bounds(None, 7), None);
}

#[test]
fn preferred_bounds_falls_back_to_the_other_source() {
    let card = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
    let inspector = Bounds::new(point(px(20.), px(20.)), size(px(30.), px(30.)));
    assert_eq!(preferred_bounds(true, Some(card), Some(inspector)), Some(card));
    assert_eq!(preferred_bounds(true, None, Some(inspector)), Some(inspector));
    assert_eq!(preferred_bounds(false, Some(card), Some(inspector)), Some(inspector));
    assert_eq!(preferred_bounds(false, Some(card), None), Some(card));
}

#[test]
fn motion_durations_are_ordered() {
    assert!(MOTION_FAST < MOTION_BASE);
    assert!(MOTION_BASE < MOTION_SLOW);
    assert!(MOTION_SLOW < MOTION_MORPH);
}
```

- [ ] **Step 2: Run the tests and verify they fail**

Run: `cargo test -p vrc-photo-ui nav_index_matches_sidebar_order`
Expected: compile error because `nav_index`, the constants, and the other helpers do not exist.

- [ ] **Step 3: Add the motion constants**

Place them next to the existing duration constants (`CARD_INTRO_SECS`, etc.). Use `Duration::from_millis`, which is const:

```rust
const MOTION_FAST: Duration = Duration::from_millis(160);
const MOTION_BASE: Duration = Duration::from_millis(220);
const MOTION_SLOW: Duration = Duration::from_millis(300);
const MOTION_MORPH: Duration = Duration::from_millis(340);
const MOTION_FPS: f32 = 60.0;
const SCREEN_SLIDE: f32 = 14.0;
const VIEWER_NAV_SLIDE: f32 = 28.0;
const NAV_ITEM_HEIGHT: f32 = 40.0;
const NAV_ITEM_GAP: f32 = 3.0;
```

- [ ] **Step 4: Implement the helpers**

Place `nav_index` through `screen_direction` near the other navigation helpers. Place `lerp_bounds`, `eased_progress`, `morph_bounds_at`, and `viewer_image_rect` near `ease_motion()` / `viewer_image_box()`. `nav_bar_left` returns `5.0` when collapsed and `-1.0` otherwise.

`viewer_image_rect`:

```rust
fn viewer_image_rect(viewport: Size<Pixels>, details_open: bool, aspect: f32) -> Bounds<Pixels> {
    let (width, height) = viewer_image_box(
        viewport.width.into(),
        viewport.height.into(),
        details_open,
        aspect,
    );
    let details = if details_open { VIEWER_DETAILS_WIDTH } else { 0.0 };
    let center_x = (f32::from(viewport.width) - details) / 2.0;
    let center_y = VIEWER_TOP_BAR + (f32::from(viewport.height) - VIEWER_TOP_BAR) / 2.0;
    Bounds::new(
        point(px(center_x - width / 2.0), px(center_y - height / 2.0)),
        size(px(width), px(height)),
    )
}
```

`eased_progress` must return `1.0` for a zero duration; otherwise clamp the elapsed ratio to `0..=1`, run it through `ease_motion`, and return it.

`morph_bounds_at` is `lerp_bounds(from, to, eased_progress(started_at, MOTION_MORPH, now))`.

- [ ] **Step 5: Run the tests and verify they pass**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass, including the eight new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add motion geometry helpers"
```

---

### Task 2: Sidebar Active Indicator

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields and `new()`, `switch_screen()`, `sidebar()`, `nav_item()`, `nav_tooltip()`).

**Interfaces:**
- Consumes: `nav_index`, `nav_item_top`, `nav_bar_left`, `MOTION_FAST`, `MOTION_BASE`, `MOTION_SLOW`, `MOTION_FPS`, `ease_motion`, `lerp_color`.
- Produces: `App.nav_previous: Screen`, `App.nav_epoch: u64`, and a shared absolute nav highlight; `nav_item()` no longer draws the active background or bar.

- [ ] **Step 1: Add the state fields**

In `struct App`, next to `sidebar_motion_epoch`:

```rust
nav_previous: Screen,
nav_epoch: u64,
```

Initialize both in `App::new`: `nav_previous: Screen::Photos`, `nav_epoch: 0`.

- [ ] **Step 2: Record the previous tab in `switch_screen()`**

At the top of `switch_screen()`, before assigning `self.screen = screen`:

```rust
if self.screen != screen {
    self.nav_previous = self.screen;
    self.nav_epoch = self.nav_epoch.wrapping_add(1);
}
```

Do not increment when the user clicks the already-active tab.

- [ ] **Step 3: Replace the per-item indicator with a shared highlight**

In `sidebar()`, replace the direct list of `self.nav_item(...)` children with a relative nav column:

```rust
.child(
    v_flex()
        .relative()
        .w_full()
        .gap(px(NAV_ITEM_GAP))
        .child(self.nav_highlight())
        .child(self.nav_item("写真", Screen::Photos, IconName::GalleryVerticalEnd, Some(photo_count.clone()), cx))
        .child(self.nav_item("検索", Screen::Search, IconName::Search, None, cx))
        .child(self.nav_item("圧縮", Screen::Compression, IconName::Minimize, compression_count, cx))
        .child(self.nav_item("ダッシュボード", Screen::Dashboard, IconName::LayoutDashboard, None, cx))
        .child(self.nav_item("設定", Screen::Settings, IconName::Settings, update_badge, cx)),
)
```

Keep the existing 12 px spacer before this column. The column's first item top is then 0 relative to the column.

Add `fn nav_highlight(&self) -> AnyElement`:

- `from = nav_item_top(nav_index(self.nav_previous))`, `to = nav_item_top(nav_index(self.screen))`.
- Pill: absolute, `left_0().right_0()`, height `NAV_ITEM_HEIGHT`, radius 11 px, `bg(rgb(ACCENT_SOFT))`, `top` animated from `from` to `to` over `MOTION_BASE` (zero duration when `nav_epoch == 0`), keyed by `("nav-pill", nav_epoch)`.
- Bar: absolute, `left_0()`, width 2 px, height 18 px, top animated to `from + 11` -> `to + 11` with the same animation. Its child is a 2 px wide blue bar with `left` animated from `nav_bar_left(!collapsed)` to `nav_bar_left(collapsed)` over `MOTION_SLOW`, keyed by `("nav-bar-x", sidebar_motion_epoch)`.
- Both animations use `.with_easing(ease_motion).with_max_fps(MOTION_FPS)`.

- [ ] **Step 4: Remove the old active drawing and animate item colors**

In `nav_item()`:

- Remove `.when(active, |this| this.bg(rgb(ACCENT_SOFT)).text_color(rgb(0xeaf4ff)))`.
- Remove the `.when(active, |this| this.child(...bar...))` block.
- Keep the inactive hover behavior.
- Replace the active/inactive icon branch with one `Icon` wrapped in a color animation keyed by `("nav-icon", label, nav_epoch)`: from `BLUE` to `MUTED` when the item was active, from `MUTED` to `BLUE` when it becomes active, otherwise `MUTED`.
- Wrap the row in a text-color animation keyed by `("nav-text", label, nav_epoch)`: from `0xeaf4ff` to `MUTED` when it was active, `MUTED` to `0xeaf4ff` when it becomes active, otherwise `MUTED`. Keep the existing hover override.
- Animate the count color keyed by `("nav-count", label, nav_epoch)`: `BLUE`/`MUTED_2`.
- All color animations use `MOTION_FAST` and `ease_motion`.

- [ ] **Step 5: Animate the collapsed-item alignment**

Replace the instant `.when(collapsed, justify_center)` / `.when(!collapsed, px(11.).gap(11.))` on the row with an inner content wrapper:

```rust
let from_pad = if collapsed { 11.0 } else { 18.5 };
let to_pad = if collapsed { 18.5 } else { 11.0 };
let from_gap = if collapsed { 11.0 } else { 0.0 };
let to_gap = if collapsed { 0.0 } else { 11.0 };
let pad_anim = Animation::new(if epoch == 0 { Duration::ZERO } else { MOTION_SLOW })
    .with_easing(ease_motion)
    .with_max_fps(MOTION_FPS);
let content = h_flex()
    .w_full()
    .items_center()
    .with_animation(
        ("nav-pad", label, epoch),
        pad_anim,
        move |el, t| el.pl(px(from_pad + (to_pad - from_pad) * t)).gap(px(from_gap + (to_gap - from_gap) * t)),
    )
    .child(icon_element)
    .child(label_block);
```

The row keeps `.h(px(NAV_ITEM_HEIGHT)).w_full().items_center()`, its `.id(label)`, click handler, and hover handler. The icon now travels to the optical center instead of snapping alignment modes. Use `nav_bar_left()` and `NAV_ITEM_HEIGHT`/`NAV_ITEM_GAP` instead of literal `5.0`, `-1.0`, `40.0`, and `3.0` where those values describe the indicator geometry.

- [ ] **Step 6: Animate the collapsed tooltip**

In `nav_tooltip()`, replace the index `match` with `nav_index(screen)` and wrap the returned tooltip in:

```rust
.with_animation(
    ("nav-tooltip", index),
    Animation::new(MOTION_FAST).with_easing(ease_motion).with_max_fps(MOTION_FPS),
    move |el, t| el.opacity(t).relative().left(px(-4.0 * (1.0 - t))),
)
```

- [ ] **Step 7: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass.

- [ ] **Step 8: Visual check**

Run the demo app. Switch through all five tabs in both expanded and collapsed sidebar modes. Confirm the pill and blue bar travel together, the bar stays attached during collapse, labels/icons cross-fade, and no row content snaps.

- [ ] **Step 9: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: slide the sidebar active indicator"
```

---

### Task 3: Screen Enter Transition

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields and `new()`, `switch_screen()`, `content()`).

**Interfaces:**
- Consumes: `screen_direction`, `SCREEN_SLIDE`, `MOTION_BASE`, `MOTION_FPS`, `ease_motion`.
- Produces: `App.screen_epoch: u64`, `App.screen_direction: f32`; every screen swap gets a directional enter animation.

- [ ] **Step 1: Add the state fields**

In `struct App`:

```rust
screen_epoch: u64,
screen_direction: f32,
```

Initialize `screen_epoch: 0`, `screen_direction: 1.0`.

- [ ] **Step 2: Update `switch_screen()`**

After the nav state update from Task 2, still before assigning the new screen:

```rust
self.screen_direction = screen_direction(self.screen, screen);
self.screen_epoch = self.screen_epoch.wrapping_add(1);
```

Only do this inside the `if self.screen != screen` branch. The first render has epoch 0 and must not animate.

- [ ] **Step 3: Wrap only the screen element in `content()`**

Change `content()` so the global alert bars stay static and the screen is wrapped:

```rust
let screen = /* existing match */;
let enter = Animation::new(if self.screen_epoch == 0 { Duration::ZERO } else { MOTION_BASE })
    .with_easing(ease_motion)
    .with_max_fps(MOTION_FPS);
let direction = self.screen_direction;
let screen = v_flex()
    .flex_1()
    .min_h_0()
    .child(screen)
    .with_animation(("screen-enter", self.screen_epoch), enter, move |el, t| {
        el.relative()
            .left(px(direction * SCREEN_SLIDE * (1.0 - t)))
            .opacity(t)
    });
v_flex()
    .flex_1()
    .min_h_0()
    .children(self.global_alerts(cx))
    .child(screen)
    .into_any_element()
```

- [ ] **Step 4: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass.

- [ ] **Step 5: Visual check**

Run the demo. Move Photos -> Search -> Compression -> Dashboard -> Settings and back. Confirm each screen enters from the correct side, the alert bars do not move, and the first frame at startup is static.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: animate screen changes"
```

---

### Task 4: Image Bounds Capture

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields and `new()`, `render()`, `photo_card()`, `inspector()` and its `preview_frame`).

**Interfaces:**
- Consumes: `current_tagged_bounds`, `canvas`, `Bounds<Pixels>`.
- Produces:
  - `App.render_epoch: u64`
  - `App.card_image_bounds: Rc<RefCell<HashMap<i64, (u64, Bounds<Pixels>)>>>`
  - `App.inspector_preview_bounds: Rc<RefCell<Option<(u64, Bounds<Pixels>)>>>`

- [ ] **Step 1: Add the state fields and initialize them**

In `struct App`:

```rust
render_epoch: u64,
card_image_bounds: Rc<RefCell<HashMap<i64, (u64, Bounds<Pixels>)>>>,
inspector_preview_bounds: Rc<RefCell<Option<(u64, Bounds<Pixels>)>>>,
```

Initialize `render_epoch: 0`, both maps with `Rc::new(RefCell::new(...))`. Add `RefCell` to the `std::cell` import.

- [ ] **Step 2: Increment the render epoch**

At the very start of `impl Render for App::render()`:

```rust
self.render_epoch = self.render_epoch.wrapping_add(1);
```

This happens before any card or inspector element is built, so all prepaint closures in that frame tag their bounds with the same epoch.

- [ ] **Step 3: Capture card image bounds**

In `photo_card()`, before adding the canvas child to `thumb`, clone the map, the current `render_epoch`, and `id`. Add:

```rust
let bounds_map = self.card_image_bounds.clone();
let frame = self.render_epoch;
thumb = thumb.child(
    canvas(
        move |bounds, _, _| {
            bounds_map.borrow_mut().insert(id, (frame, bounds));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full(),
);
```

Put this after the thumbnail image child but before the status overlays, or after all overlays; it paints nothing, so order does not matter. Do not put it inside a condition that depends on the thumbnail being loaded.

- [ ] **Step 4: Capture the inspector preview bounds**

In `inspector()`, after the `preview_frame` composition and before it is added to `body`, add:

```rust
let preview_bounds = self.inspector_preview_bounds.clone();
let frame = self.render_epoch;
preview_frame = preview_frame.child(
    canvas(
        move |bounds, _, _| {
            *preview_bounds.borrow_mut() = Some((frame, bounds));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full(),
);
```

- [ ] **Step 5: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass. This task has no visible change yet.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: capture image bounds for viewer transitions"
```

---

### Task 5: Viewer Open Morph

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields and `new()`, `open_quick_look()`, `quick_look()`, `render()`, call sites at the card double-click, inspector button, and `OpenQuickLook` action; new `viewer_morph_overlay()`).

**Interfaces:**
- Consumes: all Task 1 helpers, Task 4 maps, `thumbnails.get(id)`, `preview`, `viewer_image_box`, `MOTION_MORPH`, `MOTION_FPS`.
- Produces:
  - `enum ViewerOpenSource { Card, Inspector, Auto }`
  - `struct ViewerMorph { epoch, started_at, from, to, thumbnail, preview, color, source, closing, fade_only }`
  - `App.viewer_morph: Option<ViewerMorph>`, `App.viewer_morph_epoch: u64`
  - `App.viewer_return_source: Option<ViewerOpenSource>`
  - `open_quick_look(window, cx, source: ViewerOpenSource)`
  - `ViewerMorph::current_bounds(now: Instant) -> Bounds<Pixels>`
  - `fn actual_source(id, source) -> Option<(ViewerOpenSource, Bounds<Pixels>)>`
  - `fn viewer_morph_overlay(&self) -> AnyElement`

- [ ] **Step 1: Add the failing state-machine test**

In the tests module, construct a morph with no images and assert its in-flight rectangle:

```rust
#[test]
fn viewer_morph_current_bounds_follows_the_timeline() {
    let from = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(80.)));
    let to = Bounds::new(point(px(200.), px(100.)), size(px(400.), px(300.)));
    let start = Instant::now();
    let morph = ViewerMorph {
        epoch: 1,
        started_at: start,
        from,
        to,
        thumbnail: None,
        preview: None,
        color: 0,
        source: ViewerOpenSource::Card,
        closing: false,
        fade_only: false,
    };
    assert_eq!(morph.current_bounds(start), from);
    assert_eq!(morph.current_bounds(start + MOTION_MORPH), to);
    let middle = morph.current_bounds(start + MOTION_MORPH / 2);
    assert!(middle.origin.x > from.origin.x && middle.origin.x < to.origin.x);
}
```

Run: `cargo test -p vrc-photo-ui viewer_morph_current_bounds_follows_the_timeline`
Expected: compile error because the struct and enum do not exist.

- [ ] **Step 2: Define the source enum and morph struct**

Place near `ViewerActivate` or the other view types:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewerOpenSource {
    Card,
    Inspector,
    Auto,
}

struct ViewerMorph {
    epoch: u64,
    started_at: Instant,
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    thumbnail: Option<Arc<RenderImage>>,
    preview: Option<Arc<RenderImage>>,
    color: u32,
    source: ViewerOpenSource,
    closing: bool,
    fade_only: bool,
}

impl ViewerMorph {
    fn current_bounds(&self, now: Instant) -> Bounds<Pixels> {
        if self.fade_only {
            return self.from;
        }
        morph_bounds_at(self.from, self.to, self.started_at, now)
    }
}
```

Add `viewer_morph: Option<ViewerMorph>`, `viewer_morph_epoch: u64`, and `viewer_return_source: Option<ViewerOpenSource>` to `App`; initialize `None`, `0`, `None`. The return source is set when the viewer opens and is kept for the whole viewer session, because the opening morph is cleared after ~400 ms but closing later still needs it.

- [ ] **Step 3: Add the source resolution helper**

Add a method on `App`:

```rust
fn actual_source(&self, id: i64, source: ViewerOpenSource) -> Option<(ViewerOpenSource, Bounds<Pixels>)> {
    let card = current_tagged_bounds(
        self.card_image_bounds.borrow().get(&id).copied(),
        self.render_epoch,
    );
    let inspector = current_tagged_bounds(*self.inspector_preview_bounds.borrow(), self.render_epoch);
    if source == ViewerOpenSource::Inspector {
        inspector.map(|b| (ViewerOpenSource::Inspector, b))
            .or_else(|| card.map(|b| (ViewerOpenSource::Card, b)))
    } else {
        card.map(|b| (ViewerOpenSource::Card, b))
            .or_else(|| inspector.map(|b| (ViewerOpenSource::Inspector, b)))
    }
}
```

`actual_source` records the real fallback for the return path. `ViewerOpenSource::Auto` prefers the card, as does `Card`.

- [ ] **Step 4: Change the `open_quick_look()` signature and call sites**

Change to:

```rust
fn open_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>, source: ViewerOpenSource)
```

Update the three call sites:

- card double-click (`photo_card()`): `ViewerOpenSource::Card`
- inspector `open-viewer` button: `ViewerOpenSource::Inspector`
- `OpenQuickLook` action in `render()`: `ViewerOpenSource::Auto`

- [ ] **Step 5: Build the opening morph**

In `open_quick_look()`, after the early return and before setting `quick_look_open`:

```rust
let id = self.selected.expect("selected photo");
let aspect = self
    .selected_photo
    .as_ref()
    .map(|photo| {
        let (w, h) = photo.effective_dimensions();
        if h > 0 { w as f32 / h as f32 } else { 16.0 / 9.0 }
    })
    .unwrap_or(16.0 / 9.0);
let to = viewer_image_rect(window.viewport_size(), self.quick_look_details_open, aspect);
let resolved = self.actual_source(id, source);
if let Some((actual_source, _)) = resolved {
    self.viewer_return_source = Some(actual_source);
}
if !cx.reduce_motion()
    && let Some((actual_source, from)) = resolved
{
    let thumbnail = self.thumbnails.get(id);
    let preview = self.preview.clone();
    if thumbnail.is_some() || preview.is_some() {
        self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
        let epoch = self.viewer_morph_epoch;
        let color = self
            .selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(0);
        self.viewer_morph = Some(ViewerMorph {
            epoch,
            started_at: Instant::now(),
            from,
            to,
            thumbnail,
            preview,
            color,
            source: actual_source,
            closing: false,
            fade_only: false,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(MOTION_MORPH + Duration::from_millis(60))
                .await;
            this.update(cx, |this, cx| {
                if this.viewer_morph_epoch == epoch
                    && this.viewer_morph.as_ref().is_some_and(|m| !m.closing)
                {
                    this.viewer_morph = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}
```

Do not start the timer when reduce motion is on. If no bounds or no image is available, leave `viewer_morph` as `None` and let the existing viewer fade handle the open.

- [ ] **Step 6: Hide the real viewer image area while opening**

In `quick_look()`, determine:

```rust
let opening_morph = self.viewer_morph.as_ref().is_some_and(|m| !m.closing);
```

Apply `.opacity(if opening_morph { 0.0 } else { 1.0 })` to the **image area** (`div().id("viewer-image-area")`), not to the inner `image` element. The inner element still has the existing per-photo fade animation in this task, and an outer opacity is the only reliable way to hide both the image and the placeholder until the overlay hands off. Keep the click-to-close handler active; it does not depend on visibility.

When the cleanup timer fires, the opacity returns to 1 and the real image appears at the target rectangle.

- [ ] **Step 7: Render the morph overlay**

Add `fn viewer_morph_overlay(&self) -> AnyElement`:

- Return an empty `div()` when `self.viewer_morph` is `None`.
- Root: absolute, `size_full()`, no background, no `occlude()`, no handlers.
- Frame: absolute, `overflow_hidden`, `bg(rgb(color))`, animated with `("viewer-morph-frame", epoch)` over `MOTION_MORPH`, `ease_motion`, `MOTION_FPS`. In the closure, compute `lerp_bounds(from, to, t)` and set `left/top/w/h`; interpolate the radius from 12 px to 10 px with the same `t`.
- Old layer: absolute `size_full`, `gpui_kit::img(thumbnail)` with `ObjectFit::Cover` for opening. Wrap in `with_animation(("viewer-morph-old", epoch), ...)` with opacity `1.0 - t` and the interpolated radius. When `closing` is true, use `Contain` instead of `Cover`.
- New layer: same, with `ObjectFit::Contain` for opening, opacity `t`. When `closing` is true, use `Cover` (this is the close task? For now, this task only renders opening; implement the flags so Task 6 can flip them).
- If the preview is missing, use the thumbnail for both layers. If the thumbnail is missing, use the preview for both. If both are missing, do not render a frame. A thumbnail-only morph still hands off to the viewer's placeholder if the preview decode is still pending; that is an accepted brief state, not a reason to skip the motion.
- Do not add `.on_click`, `.occlude`, or a hitbox.

In `render()`, append `.child(self.viewer_morph_overlay())` after the quick-look overlay and after the manual-compression overlay, so it is the topmost element.

- [ ] **Step 8: Run the test and regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: the new bounds test and all existing tests pass.

- [ ] **Step 9: Visual check**

Run the demo. Double-click a card. Confirm the thumbnail starts exactly over the card and grows into the viewer rectangle, then the real image takes over without a jump. Confirm the viewer chrome fades in underneath. Close with Esc; the close morph is not implemented yet, so only verify that the viewer closes cleanly and no overlay is left behind after ~400 ms.

- [ ] **Step 10: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: morph the viewer open from its source image"
```

---

### Task 6: Viewer Close Morph and Source-Aware Return

**Files:**
- Modify: `crates/ui/src/main.rs` (`close_quick_look()`, `viewer_morph_overlay()`, `open_quick_look()` for interaction cancellation, `step_viewer_photo()`, `toggle_quick_look_details()`, `render()`).

**Interfaces:**
- Consumes: Task 5's `ViewerMorph`, `current_bounds`, `actual_source`, `viewer_image_rect`, `MOTION_MORPH`.
- Produces: a closing morph that returns to the card or inspector preview, and a fade fallback when the source is unavailable.

- [ ] **Step 1: Pin the close-target preference**

Add this regression test (it should pass immediately because `preferred_bounds` already exists; it pins the rule the closing code must use):

```rust
#[test]
fn closing_prefers_the_original_source() {
    let card = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
    let inspector = Bounds::new(point(px(20.), px(20.)), size(px(30.), px(30.)));
    assert_eq!(preferred_bounds(true, Some(card), Some(inspector)), Some(card));
    assert_eq!(preferred_bounds(false, Some(card), Some(inspector)), Some(inspector));
}
```

Run: `cargo test -p vrc-photo-ui closing_prefers_the_original_source`
Expected: PASS.

- [ ] **Step 2: Capture the closing start rectangle**

In `close_quick_look()`, before setting `quick_look_open = false`, compute:

```rust
let photo_aspect = self
    .selected_photo
    .as_ref()
    .map(|photo| {
        let (w, h) = photo.effective_dimensions();
        if h > 0 { w as f32 / h as f32 } else { 16.0 / 9.0 }
    })
    .unwrap_or(16.0 / 9.0);
let viewer_rect = viewer_image_rect(
    window.viewport_size(),
    self.quick_look_details_open,
    photo_aspect,
);
```

If an opening morph is still in flight (`self.viewer_morph.as_ref().is_some_and(|m| !m.closing)`), use `morph.current_bounds(Instant::now())` as `from`; otherwise use `viewer_rect`.

- [ ] **Step 3: Build the closing morph**

If `cx.reduce_motion()` is true, skip the overlay, clear `viewer_return_source`, and close normally (`quick_look_open = false`, `viewer_morph = None`).

Determine the return target:

- If an opening morph exists, use its `source`; otherwise use `self.viewer_return_source.unwrap_or(ViewerOpenSource::Auto)`. The return source was stored when the viewer opened and survives the opening-morph cleanup timer.
- Resolve the current card/inspector bounds for the current selected photo and use `preferred_bounds(source == ViewerOpenSource::Card, card_bounds, inspector_bounds)`.
- If a target exists and is current, `to = target`, `fade_only = false`.
- If no target exists, `to = from`, `fade_only = true`.

Clone the current thumbnail/preview/color, increment `viewer_morph_epoch`, set `viewer_morph = Some(ViewerMorph { closing: true, ... })`, then set `quick_look_open = false`. Clear `viewer_return_source` after the closing morph has copied the source. Start the same guarded cleanup timer as Task 5. The timer must check `m.closing` and the epoch.

If there is no opening morph and the viewer never had a source, closing can remain the existing instant close; do not create a `fade_only` overlay with no image. If both the thumbnail and preview are `None`, skip the close morph and close normally.

- [ ] **Step 4: Cancel the opening morph on next/previous and details**

At the start of `step_viewer_photo()` and `toggle_quick_look_details()`:

```rust
if self.viewer_morph.as_ref().is_some_and(|m| !m.closing) {
    self.viewer_morph = None;
}
```

This drops the overlay and reveals the real viewer image before the new animation. The old cleanup timer becomes a no-op because `viewer_morph_epoch` changes only on the next morph; the timer also checks `viewer_morph` is still present, so clearing it is enough.

- [ ] **Step 5: Extend the overlay for closing**

In `viewer_morph_overlay()`:

- When `closing`, render a full-window backdrop below the frame: `bg(rgb(0x05070a))` with a photo-color tint child, wrapped in `with_animation(("viewer-morph-backdrop", epoch), ...)` opacity `1.0 - t`.
- For closing, the old layer uses `Contain` (the viewer image), the new layer uses `Cover` (the card thumbnail).
- If `fade_only`, render only the old layer at `from` and fade it out; do not render the new layer.
- Keep the frame geometry animation even for `fade_only` (it stays at `from`).

- [ ] **Step 6: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass.

- [ ] **Step 7: Visual check**

Run the demo. Open a card and press Esc normally: confirm the image shrinks back to the same card. Open a card, press next until the original card is off-screen, then Esc: confirm a fade fallback with no stale flight. Open a card and press Esc immediately (inside ~340 ms): confirm no flash to full size. Open from the inspector "全画面表示" button with the card scrolled out of view, then Esc: confirm it returns to the inspector preview.

- [ ] **Step 8: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: morph the viewer back to its source"
```

---

### Task 7: Viewer Navigation, Aspect Animation, and Details Sync

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields and `new()`, `open_quick_look()`, `step_viewer_photo()`, `toggle_quick_look_details()`, `quick_look()`).

**Interfaces:**
- Consumes: `MOTION_BASE`, `MOTION_SLOW`, `VIEWER_NAV_SLIDE`, `viewer_image_box`, `eased_progress`, `lerp_bounds`.
- Produces:
  - `App.viewer_nav_epoch: u64`, `App.viewer_nav_direction: f32`
  - `struct ViewerBoxState` with `fn size_at(&self, t: f32, viewport: Size<Pixels>) -> (f32, f32)`
  - directional previous/next cross-fade and a smoothly resizing image frame.

- [ ] **Step 1: Write the failing box-state test**

Add:

```rust
#[test]
fn viewer_box_state_interpolates_mixed_aspects() {
    let viewport = size(px(1200.), px(800.));
    let state = ViewerBoxState {
        epoch: 1,
        started_at: Instant::now(),
        from_aspect: 0.5,
        to_aspect: 2.0,
        from_details: false,
        to_details: false,
        duration: MOTION_BASE,
    };
    let (w0, h0) = state.size_at(0.0, viewport);
    let (w1, h1) = state.size_at(1.0, viewport);
    let (wm, hm) = state.size_at(0.5, viewport);
    assert!(w0 < w1);
    assert!(h0 > h1);
    assert!(wm > w0 && wm < w1);
    assert!(hm < h0 && hm > h1);
}

#[test]
fn viewer_box_state_hits_both_details_ends() {
    let viewport = size(px(1200.), px(800.));
    let state = ViewerBoxState {
        epoch: 2,
        started_at: Instant::now(),
        from_aspect: 16.0 / 10.0,
        to_aspect: 16.0 / 10.0,
        from_details: false,
        to_details: true,
        duration: MOTION_SLOW,
    };
    let (closed, _) = state.size_at(0.0, viewport);
    let (open, _) = state.size_at(1.0, viewport);
    assert!(closed > open);
}
```

Run: `cargo test -p vrc-photo-ui viewer_box_state_interpolates_mixed_aspects`
Expected: compile error because `ViewerBoxState` does not exist.

- [ ] **Step 2: Add the state fields and `ViewerBoxState`**

In `App`:

```rust
viewer_nav_epoch: u64,
viewer_nav_direction: f32,
viewer_box: ViewerBoxState,
```

Initialize `viewer_nav_epoch: 0`, `viewer_nav_direction: 1.0`, and a zero-duration box state with the current photo aspect and details state (or aspect `16.0/10.0` before a photo is selected).

Define:

```rust
#[derive(Clone, Copy)]
struct ViewerBoxState {
    epoch: u64,
    started_at: Instant,
    from_aspect: f32,
    to_aspect: f32,
    from_details: bool,
    to_details: bool,
    duration: Duration,
}

impl ViewerBoxState {
    fn size_at(&self, t: f32, viewport: Size<Pixels>) -> (f32, f32) {
        let t = t.clamp(0.0, 1.0);
        let from = viewer_image_box(
            viewport.width.into(),
            viewport.height.into(),
            self.from_details,
            self.from_aspect,
        );
        let to = viewer_image_box(
            viewport.width.into(),
            viewport.height.into(),
            self.to_details,
            self.to_aspect,
        );
        (
            from.0 + (to.0 - from.0) * t,
            from.1 + (to.1 - from.1) * t,
        )
    }

    fn current_size(&self, viewport: Size<Pixels>) -> (f32, f32) {
        let t = eased_progress(self.started_at, self.duration, Instant::now());
        self.size_at(t, viewport)
    }
}
```

- [ ] **Step 3: Reset the box state on open**

In `open_quick_look()`, set `viewer_nav_epoch = 0`, `viewer_nav_direction = 1.0`, and a zero-duration box state whose from/to values both use the selected aspect and the current `quick_look_details_open`. This keeps the initial viewer static.

- [ ] **Step 4: Update the box state on next/previous**

In `step_viewer_photo()`, before `select_photo()`:

```rust
let old_aspect = /* current selected photo aspect */;
let old_details = self.quick_look_details_open;
```

After computing `next` and before `select_photo()` (or after, using the new selected photo's dimensions), set:

```rust
self.viewer_nav_epoch = self.viewer_nav_epoch.wrapping_add(1);
self.viewer_nav_direction = if delta > 0 { 1.0 } else { -1.0 };
self.viewer_box = ViewerBoxState {
    epoch: self.viewer_box.epoch.wrapping_add(1),
    started_at: Instant::now(),
    from_aspect: old_aspect,
    to_aspect: new_aspect,
    from_details: old_details,
    to_details: old_details,
    duration: MOTION_BASE,
};
```

Use the new selected photo's `effective_dimensions()` for `new_aspect`.

- [ ] **Step 5: Update the box state on details toggle**

In `toggle_quick_look_details()`:

```rust
let old_details = self.quick_look_details_open;
self.quick_look_details_open = !old_details;
let aspect = /* current selected photo aspect */;
self.viewer_box = ViewerBoxState {
    epoch: self.viewer_box.epoch.wrapping_add(1),
    started_at: Instant::now(),
    from_aspect: aspect,
    to_aspect: aspect,
    from_details: old_details,
    to_details: self.quick_look_details_open,
    duration: MOTION_SLOW,
};
```

- [ ] **Step 6: Replace the fixed image box in `quick_look()`**

Remove the direct `viewer_image_box(...)` call that sets the image width/height. Build the image frame with a starting size from the box state and wrap it:

```rust
let box_state = self.viewer_box;
let box_anim = Animation::new(box_state.duration)
    .with_easing(ease_motion)
    .with_max_fps(MOTION_FPS);
let (start_w, start_h) = box_state.size_at(0.0, window.viewport_size());
let mut image_frame = div()
    .relative()
    .flex_shrink_0()
    .w(px(start_w))
    .h(px(start_h))
    .overflow_hidden()
    .rounded(px(10.))
    .bg(rgb(color))
    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()));
image_frame = image_frame.with_animation(
    ("viewer-box", box_state.epoch),
    box_anim,
    move |el, t| {
        let (w, h) = box_state.size_at(t, window.viewport_size());
        el.w(px(w)).h(px(h))
    },
);
```

The `window` borrow inside the closure is not `'static`; compute the viewport before the closure and capture it by value:

```rust
let viewport = window.viewport_size();
let box_state = self.viewer_box;
let anim = ...;
let frame = ... .with_animation(..., move |el, t| {
    let (w, h) = box_state.size_at(t, viewport);
    el.w(px(w)).h(px(h))
});
```

- [ ] **Step 7: Add directional cross-fade layers**

Inside the image frame:

- If `previous_preview` is `Some` and `viewer_nav_epoch > 0`, add the old layer as an absolute `size_full` image with `ObjectFit::Contain`, rounded 10 px, and `with_animation(("viewer-prev", viewer_nav_epoch), MOTION_BASE, ...)`:
  - opacity `1.0 - t`
  - `relative().left(px(-direction * VIEWER_NAV_SLIDE * t))`
- Add the current image as an absolute `size_full` layer with `ObjectFit::Contain`, rounded 10 px, and `with_animation(("viewer-current", viewer_nav_epoch), ...)`:
  - opacity `t`
  - `relative().left(px(direction * VIEWER_NAV_SLIDE * (1.0 - t)))`
- When `viewer_nav_epoch == 0`, use `Duration::ZERO` and render the current layer statically.
- If there is no preview, keep the existing "プレビューを読み込めません" placeholder inside the frame and hide the image layers.

The frame's size animation and the layers' cross-fade are independent; both share the same 220 ms duration for next/previous, so they finish together. The details toggle only changes the frame size.

- [ ] **Step 8: Animate the details body and top bar**

- Wrap the details body (the `v_flex` built inside `if details_open`) in `with_animation(("viewer-details-body", quick_look_details_epoch), Animation::new(if quick_look_details_epoch == 0 { Duration::ZERO } else { MOTION_FAST }).with_easing(ease_motion).with_max_fps(MOTION_FPS), ...)` with opacity `t` and `relative().top(px(6.0 * (1.0 - t)))`. The zero-duration case is required: a restored details state must render statically when the viewer opens, matching the panel-width behavior.
- Wrap the top bar in `with_animation(("viewer-topbar", quick_look_epoch), Animation::new(MOTION_BASE).with_easing(ease_motion).with_max_fps(MOTION_FPS), ...)` with `relative().top(px(-8.0 * (1.0 - t)))`. Do not add a second opacity fade; the root already fades.

- [ ] **Step 9: Use the in-flight box size when closing**

In `close_quick_look()`, when no opening morph is in flight, replace the raw `viewer_image_rect(...)` start rectangle with a rectangle centered on the current image area and sized by `self.viewer_box.current_size(window.viewport_size())`. Center it with the current details state (`VIEWER_DETAILS_WIDTH` or 0) and `VIEWER_TOP_BAR`, matching `viewer_image_rect`'s center formula. This makes closing during an aspect-ratio or details animation start from the image's visible size instead of snapping to the target size. When an opening morph is in flight, keep using its `current_bounds()`.

- [ ] **Step 10: Run the tests and regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: both new box-state tests and all existing tests pass.

- [ ] **Step 11: Visual check**

Run the demo. Step through photos: confirm opposite-direction slides and a stable details panel. If the demo library has only one aspect ratio, add a temporary local portrait image or use a real library if available; confirm the frame resizes over 220 ms. Toggle details and confirm the image shrinks/grows with the panel instead of jumping. Confirm next/previous during the opening morph cancels the overlay.

- [ ] **Step 12: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: animate viewer navigation and details"
```

---

### Task 8: Overlay and Selection Bar Entrances

**Files:**
- Modify: `crates/ui/src/main.rs` (`manual_compression_overlay()`, `global_alerts()`, `import_report_panel()`, `selection_bar()`).

**Interfaces:**
- Consumes: `MOTION_FAST`, `MOTION_BASE`, `MOTION_FPS`, `ease_motion`.
- Produces: enter animations for the manual-compression dialog, alert bars, import report, and multi-select bar.

- [ ] **Step 1: Manual compression dialog**

- Wrap the scrim (`id("manual-compression-scrim")`) in `with_animation("manual-compression-scrim", Animation::new(MOTION_FAST)... )` with opacity `t`.
- Wrap the panel in `with_animation("manual-compression-panel", Animation::new(MOTION_BASE)... )` with `relative().top(px(10.0 * (1.0 - t)))`. Do not add a second opacity fade; the scrim already fades the panel.

- [ ] **Step 2: Global alerts and import report**

- In `global_alerts()`, wrap each returned alert element in `with_animation(("global-alert", index), Animation::new(MOTION_BASE)... )` with opacity `t` and `relative().top(px(-8.0 * (1.0 - t)))`.
- At the end of `import_report_panel()`, wrap the returned panel in `with_animation("import-report", Animation::new(MOTION_BASE)... )` with opacity `t` and `relative().top(px(-8.0 * (1.0 - t)))`.

- [ ] **Step 3: Multi-select bar**

In `selection_bar()`:

- Set `.h(px(36.))` and `.overflow_hidden()` on the bar.
- Wrap it in `with_animation("selection-bar", Animation::new(MOTION_FAST)... )` with `h(px(36.0 * t))` and `opacity(t)`.
- Keep the existing `return div()` when `count <= 1`.

- [ ] **Step 4: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass.

- [ ] **Step 5: Visual check**

Run the demo. Open the manual-compression dialog, trigger an import report (use the existing demo import fixture if available), create a multi-selection, and confirm each enters smoothly. Confirm the dialog's option rows still scroll and click.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: animate overlays and selection bars"
```

---

### Task 9: Profile and Card Selection Micro-Interactions

**Files:**
- Modify: `crates/ui/src/main.rs` (`compression_screen()`, `photo_card()`).

**Interfaces:**
- Consumes: `MOTION_FAST`, `MOTION_FPS`, `ease_motion`, `lerp_color`.
- Produces: animated profile card selection, profile editor cross-fade, and card selection badge entrance.

- [ ] **Step 1: Profile card background and border**

In the profile list, wrap each `glass_panel().id(...)` with:

```rust
.with_animation(
    SharedString::from(format!("profile-card-{}-{active}", profile.id)),
    Animation::new(MOTION_FAST).with_easing(ease_motion).with_max_fps(MOTION_FPS),
    move |el, t| {
        let (bg_from, bg_to) = if active { (PANEL, ACCENT_SOFT) } else { (ACCENT_SOFT, PANEL) };
        let (border_from, border_to) = if active { (BORDER, BLUE) } else { (BLUE, BORDER) };
        el.bg(rgb(lerp_color(bg_from, bg_to, t)))
            .border_color(rgb(lerp_color(border_from, border_to, t)))
    },
)
```

Keep the click handler and ID on the panel. The key changes when the selected card changes, so only the two affected cards animate.

- [ ] **Step 2: Profile editor cross-fade**

Wrap the profile editor with a keyed animation:

```rust
let profile_index = self.profile_index;
this.child(
    v_flex()
        .child(self.profile_editor(&profile, window, cx))
        .with_animation(
            ("profile-editor", profile_index),
            Animation::new(MOTION_FAST).with_easing(ease_motion).with_max_fps(MOTION_FPS),
            |el, t| el.opacity(t).relative().top(px(4.0 * (1.0 - t))),
        ),
)
```

`compression_screen()` and `profile_editor()` currently take `window` (the v0.5.0 suffix editor changed this). Keep that signature.

- [ ] **Step 3: Card selection badge**

Wrap the selected photo's check badge in:

```rust
.with_animation(
    ("card-check", id),
    Animation::new(MOTION_FAST).with_easing(ease_motion).with_max_fps(MOTION_FPS),
    |el, t| el.opacity(t).relative().top(px(-3.0 * (1.0 - t))),
)
```

The badge mounts only while selected, so each selection plays the entrance. Deselection remains immediate.

- [ ] **Step 4: Run the regression suite**

Run: `cargo test -p vrc-photo-ui`
Expected: all tests pass.

- [ ] **Step 5: Visual check**

Run the demo. Select different compression profiles and confirm the editor and the two affected cards transition smoothly. Select/deselect cards and confirm the check badge pops in without affecting card layout.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: animate profile and card selection states"
```

---

### Task 10: Final Verification

**Files:**
- No product files expected unless verification finds a defect.
- Do **not** include `docs/ui.md` or any design/plan document in the feature PR. The repo requires docs-only changes to ship in a separate docs PR.

**Interfaces:**
- Consumes: all previous tasks.
- Produces: a fully verified feature branch ready for PR.

- [ ] **Step 1: Run formatting, lint, and tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all three commands exit 0.

- [ ] **Step 2: Full manual pass**

Run the demo and repeat the spec's manual checklist:

1. all five tabs, forward and back, expanded and collapsed;
2. sidebar collapse on several tabs;
3. card open/close, next/previous, details toggle;
4. inspector-launched viewer with the card scrolled out;
5. Esc within the first 340 ms;
6. off-screen close fallback;
7. mixed aspect ratios;
8. reduce motion on/off;
9. narrow/wide window sizes;
10. manual-compression dialog, import report, multi-select, profile switch, card select.

- [ ] **Step 3: Commit any fixes**

If step 1 or 2 required code fixes:

```bash
git add crates/ui/src/main.rs
git commit -m "fix: polish motion edge cases"
```

If no fixes were needed, this task has no commit; the branch is ready.

---

### Task 11: Docs-Only Follow-Up (separate branch and PR)

**Files:**
- Create the docs branch from an updated `main` in its own worktree.
- Modify: `docs/ui.md` (section 4, "モーションとアクセシビリティ").
- The design and plan documents from `docs/superpowers/specs` and
  `docs/superpowers/plans` also belong in this docs-only PR, not the feature PR.

**Interfaces:**
- Consumes: the merged feature behavior.
- Produces: updated user-facing motion documentation.

- [ ] **Step 1: Create a docs worktree from updated main**

```bash
git fetch origin
git worktree add ../vrc-photo-manager-docs-motion -b docs/motion-transitions origin/main
```

Do not edit `main` in place.

- [ ] **Step 2: Add the approved design and plan**

Copy `2026-10-25-motion-transitions-design.md` and
`2026-10-25-motion-transitions.md` into the docs worktree at their
`docs/superpowers/specs/` and `docs/superpowers/plans/` paths.

- [ ] **Step 3: Update `docs/ui.md` section 4**

- Add the new durations: screen enter 220 ms, sidebar indicator 220 ms, colors/tooltips/badges 160 ms, viewer morph 340 ms, viewer navigation 220 ms.
- Describe the shared sidebar pill/bar, direction-aware screen enter, card -> viewer -> card morph with inspector fallback, directional viewer navigation, and image-box/details synchronization.
- Keep the existing statement that `with_animation` renders the final state under "アニメーションを減らす"; add that the viewer morph timer is skipped under reduce motion.
- Keep the existing 60 fps cap and no-scale/no-rotate statements.

- [ ] **Step 4: Commit and open the docs PR**

```bash
git add docs/superpowers docs/ui.md
git commit -m "docs: document motion and view transitions"
git push -u origin docs/motion-transitions
gh pr create --title "docs: motion and view transitions" --body "Design, plan, and UI documentation for the motion pass."
```

Follow the repo's `ci-watch` skill until the docs-only checks are green, then merge. This PR must not contain code changes.

- [ ] **Step 5: Clean up**

After merge, switch the main checkout to `main`, pull, delete the docs branch
locally and remotely, and remove the docs worktree.

---

## Self-Review

- **Spec coverage:** 5.1 -> Task 1; 5.2 -> Task 2; 5.3 -> Task 3; 5.4/5.5 -> Task 4; 5.6 -> Tasks 5-6; 5.7 -> Task 7; 5.8 -> Tasks 8-9; 5.9 -> Tasks 5-6; testing/docs -> Task 10.
- **Type consistency:** `ViewerOpenSource`, `ViewerMorph`, `ViewerBoxState`, `size_at`, `current_bounds`, `source_bounds`, and `actual_source` are defined before their consumers. `nav_index`, `nav_item_top`, `nav_bar_left`, `screen_direction`, `lerp_bounds`, `eased_progress`, `morph_bounds_at`, `viewer_image_rect`, `current_tagged_bounds`, and `preferred_bounds` are all defined in Task 1.
- **Review Focus tests:** mid-morph interactions are covered by Task 6's manual step; off-screen close by Task 6 step 7; mixed aspect by Task 7's unit and manual checks; reduce motion by Task 5's `cx.reduce_motion()` guard and Task 10's manual pass; rapid sidebar changes by Task 2's visual check.
- **Proportion:** the plan names signatures, test assertions, and file locations; implementation bodies are left to the builder except for algorithms the tests do not determine.

## Implementation adjustments (2026-10-25)

The whole-branch review changed these details during execution; the design's
post-review section is authoritative.

- `ViewerBoxState` became `from_size + to_aspect + to_details_width` instead of
  aspect/detail booleans, so a transition retargets from the visible size.
- Added `ViewerPanelState` for fractional details width. The panel, close
  origin, and image box all retarget from the current width.
- Nav image layers are state-driven (`viewer_nav_started_at` + `nav_t`) with a
  zero-size frame driver; they are no longer `with_animation` descendants of the
  box animation.
- Added `morph_image_plan` (card/inspector source and target, Cover/Contain,
  mirrored radii) plus tests. Opening keeps the old layer at 0.35 opacity as a
  safe underlay.
- `fade_only` closes fade the frame itself; closing morphs store
  `backdrop_from` from the viewer's open fade.
- Opening clears any closing morph; `guard_viewer_selection` clears morph
  state.
- Sidebar collapse resets `nav_previous`; the bar x animation is now the outer
  wrapper and y is the inner bar.
- The card check-badge entrance animation was dropped (it could leave the
  badge invisible); the badge is static.
- Interactive QA remains manual where the environment cannot deliver synthetic
  input to the XWayland window.
