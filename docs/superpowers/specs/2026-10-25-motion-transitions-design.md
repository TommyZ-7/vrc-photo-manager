# Motion and View-Transition Polish: Design

- Date: 2026-10-25
- Status: Approved
- Target: `crates/ui/src/main.rs`
- Related: existing "アニメーションを減らす" (`ui_reduce_motion`) setting

## 1. Intent

The app currently animates card hover, card intro, the inspector, and the
sidebar collapse, but most *navigation* still cuts from one state to the next.
The user wants the whole app to feel spatially continuous:

- the sidebar's active-item marker should move as one object between tabs;
- screen changes should stop appearing as instant swaps;
- the photo viewer should open and close like the browser View Transitions API:
  the grid image grows into the viewer image, and shrinks back on close.

The same pass should fix transitions that currently jump: the viewer image
resizes instantly when the details panel opens, and the details content appears
without a transition.

Why: navigation is the most repeated interaction in the app. Continuity makes
the current location obvious and makes the app feel deliberate rather than
flickering between screens.

## 2. Current behavior

- `sidebar()` / `nav_item()` draw the active background and the 2 px blue bar
  *inside each item*. The bar exists only on the active item and is created /
  destroyed on tab change, so it cannot move.
- `content()` selects one screen element and swaps it in immediately.
- The photo grid captures no window-space bounds, so the viewer cannot know
  where the clicked image is.
- `open_quick_look()` mounts the viewer, which fades its whole root in over
  240 ms. The viewer image fades independently over 220 ms.
- `step_viewer_photo()` replaces the image; the old image disappears at once
  and the new one fades in from zero opacity.
- `quick_look_details_open` flips a boolean. The panel width animates over
  300 ms, but `viewer_image_box()` reads the boolean, so the image jumps to its
  smaller size on the first frame while the panel is still collapsing open.
- The manual-compression dialog, import report, alert bars, multi-select bar,
  nav tooltip, and compression profile editor switch mount instantly.
- `with_animation` already renders the final state when
  `App::reduce_motion()` is set, so the existing reduce-motion setting covers
  new `with_animation` work automatically. Custom timers must check it.

## 3. Goals and non-goals

### Goals

- One shared sidebar highlight and bar; both slide to the new item on tab
  change, and the bar stays attached during sidebar collapse.
- Animate icon, label, and count colors as the active item changes.
- Give every screen change a short directional enter transition: content
  fades in while sliding a few pixels in the direction implied by the sidebar
  order.
- Implement a live shared-element morph for the viewer:
  - opening: card image bounds -> viewer image bounds, with a cross-fade from
    the cached thumbnail to the high-resolution preview;
  - closing: viewer bounds -> the current card bounds, with the reverse
    cross-fade; fall back to a short fade when the card is no longer visible.
- Make viewer next/previous directional: the outgoing image slides and fades
  out; the incoming image slides in from the other side.
- Animate the viewer image box in step with the details panel so it no longer
  jumps.
- Add entrance motion to the manual-compression dialog, import report, global
  alerts, multi-select bar, and nav tooltip.
- Animate compression-profile selection and the photo-card selection badge.
- Respect the existing reduce-motion setting and keep 60 fps caps on long
  animations.

### Non-goals

- No new dependencies, no custom GPU shaders, no whole-window snapshots.
- No animation of grid layout/reflow, scrolling, or window resizing.
- No changes to data models, database schema, or settings.
- No change to keyboard shortcuts, focus order, or hit-testing semantics.
- No full-page View Transition (the old screen is not retained as a snapshot);
  only the photo image is shared between the grid and the viewer.
- No animation of the year-rail hover expansion in this pass (possible
  follow-up; its custom paint path would need its own progress clock).

## 4. Approaches considered

### A. Live shared-element morph using cached images (chosen)

Capture each card image's window-space bounds during prepaint, then render a
temporary absolutely positioned layer that interpolates between those bounds
and the computed viewer image rectangle. The layer cross-fades the cached
thumbnail (cover) and the decoded preview (contain).

Why: GPUI exposes the bounds needed for this, and the app already owns both
images. It gives the requested continuous card -> viewer -> card movement with
one extra overlay element and no new dependencies.

Trade-off: the overlay is built from the live images rather than a rasterized
snapshot, so the card's border/status chrome is not part of the morph. The
image itself matches at both endpoints, which is what the eye follows.

### B. Viewer-only zoom/fade

Keep the current fade and add a scale/slide to the viewer image. This is much
smaller, but it does not connect the grid card to the viewer, so it does not
meet the View Transitions request. Rejected.

### C. Whole-window snapshot / scene capture

GPUI's `Window::render_to_image()` is compiled only for tests and captures the
whole window, not a subtree. Building a snapshot-based View Transition would
require a platform render target and texture upload path. Rejected as
disproportionate for this app.

### D. Retain the old screen for a cross-fade

Keeping both screen trees mounted would duplicate data fetching, focus
handling, and shortcut contexts, and would require a second live render of both
screens. A short directional enter transition gives most of the perceived
continuity at a fraction of the risk. Chosen for screens; not used for the
viewer image, where the shared-element morph is cheap.

## 5. Design

### 5.1 Motion tokens and helpers

Add a small set of constants so the new motion does not scatter magic numbers:

- `MOTION_FAST` = 160 ms: colors, badges, tooltips.
- `MOTION_BASE` = 220 ms: screen enter, viewer chrome, image cross-fade.
- `MOTION_SLOW` = 300 ms: sidebar/inspector/details panel.
- `MOTION_MORPH` = 340 ms: card <-> viewer image.
- `SCREEN_SLIDE` = 14 px; `VIEWER_NAV_SLIDE` = 28 px.
- `MOTION_FPS` = 60.

Use the existing `ease_motion` curve for all of these. Add pure helpers:

- `nav_index(Screen) -> usize` in sidebar order
  (Photos, Search, Compression, Dashboard, Settings).
- `nav_item_top(index) -> f32` (`index * (40 + 3)`).
- `screen_direction(from, to) -> f32` (`+1` when moving down the sidebar,
  `-1` upward; any non-zero sign is acceptable for equal indices).
- `lerp_bounds(from, to, t) -> Bounds<Pixels>`.
- `viewer_image_rect(viewport, details_open, aspect) -> Bounds<Pixels>`,
  derived from `viewer_image_box()` and centered in the area below the top bar.

`viewer_image_box()` stays the single source of truth for the viewer image
size; `viewer_image_rect()` only adds the centered origin.

### 5.2 Sidebar active indicator

Replace the per-item active background/bar with two shared elements inside a
relative nav column:

- **Highlight pill**: absolute, full nav width, 40 px high, 11 px radius,
  `ACCENT_SOFT`.
- **Blue bar**: absolute, 2 x 18 px, radius full, `BLUE`, offset 11 px from the
  top of the pill. Keep the current left edge (`-1` px expanded; `5` px
  collapsed) so the existing visual language does not move.

Vertical position uses `nav_item_top(nav_index(active))`. A new
`nav_previous: Screen` and `nav_epoch: u64` are updated in `switch_screen()`.
The pill and bar animate `top()` from the previous index's offset to the new
one over `MOTION_BASE`, keyed by `nav_epoch`. The blue bar's horizontal offset
animates with the existing `sidebar_motion_epoch` (`-1 <-> 5`) over
`MOTION_SLOW`, so it stays attached while the sidebar collapses.

The items themselves become transparent. `nav_item()` receives the previous
screen so it can animate the active/inactive color swap:

- icon: `MUTED <-> BLUE`
- label: `MUTED <-> 0xeaf4ff`
- count: `MUTED_2 <-> BLUE`

Use `with_animation(("nav-color", label, nav_epoch), MOTION_FAST, ...)` and
`lerp_color`; the previous active item fades back while the new one fades in.
The existing label width/opacity reveal for collapse is unchanged.

To remove a smaller jump, animate the nav item's inner padding and gap during
collapse together with `sidebar_motion_epoch`: expanded
`pl 11 / gap 11 / pr 11`, collapsed `pl 18.5 / gap 0 / pr 18.5`. This keeps the
icon travelling to the center instead of snapping alignment mode.

`nav_tooltip()` gains a `MOTION_FAST` fade + 4 px slide, keyed by the hovered
screen index so moving between collapsed icons replays it.

### 5.3 Screen transition

Add `screen_epoch: u64` and `screen_direction: f32` to `App`. In
`switch_screen()`, when the screen actually changes:

- `screen_direction = screen_direction(self.screen, screen)`;
- `screen_epoch += 1`;
- then assign the new screen.

`content()` wraps only the screen element (not the global alert bars) in a
`v_flex` and applies:

```
opacity: 0 -> 1
relative left: direction * SCREEN_SLIDE -> 0
```

over `MOTION_BASE`, keyed by `screen_epoch`. The first render (`epoch == 0`)
uses `Duration::ZERO`. The old screen is not retained; the dark window
background bridges the swap. 14 px is intentionally small: this is a
continuity cue, not a page transition.

### 5.4 Image bounds capture

Add to `App`:

- `render_epoch: u64`, incremented at the start of `render()`;
- `card_image_bounds: Rc<RefCell<HashMap<i64, (u64, Bounds<Pixels>)>>>`;
- `inspector_preview_bounds: Rc<RefCell<Option<(u64, Bounds<Pixels>)>>>`.

Each card's thumbnail and the inspector preview frame get an invisible
`canvas()` child:

- prepaint callback writes `(render_epoch_at_element_build, bounds)`;
- paint callback does nothing;
- styled `.absolute().size_full()` so it captures the content box inside the
  frame's border.

The frame tag makes stale off-screen entries detectable: opening/closing only
uses an entry whose tag equals the current `render_epoch`. Grid prepaint runs
for visible rows even while the viewer is open, so the current card stays
tagged; scrolled-away cards keep older tags and are treated as unavailable.

### 5.5 Viewer entry sources

`open_quick_look()` takes a source preference:

- `Card`: double-click on a grid card.
- `Inspector`: the inspector's "全画面表示" button.
- `Auto`: the `OpenQuickLook` action / keyboard path; prefers the card, then
  the inspector preview.

The opening morph uses the tagged preferred source, then falls back to the
other tagged source. This keeps the inspector path continuous when its card is
scrolled out of view, instead of falling back to a plain fade. The app stores
the resolved source for the whole viewer session, because the opening morph is
removed after ~400 ms and closing later still needs to know where to return.
Closing uses the same preference order: return to the inspector preview for an
inspector-launched viewer, otherwise to the card. If neither source is
currently visible, the close falls back to a fade.

### 5.6 Viewer shared-element morph

Add:

```
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
```

`App::viewer_morph: Option<ViewerMorph>` and `viewer_morph_epoch: u64`.

**Open.** In `open_quick_look()`, before setting `quick_look_open`:

1. Resolve the source bounds by the preference order in 5.5; require the
   current `render_epoch` tag.
2. Compute `to = viewer_image_rect(viewport, quick_look_details_open, aspect)`.
3. If bounds exist, `cx.reduce_motion()` is false, and at least one image is
   available, set `viewer_morph = Some(ViewerMorph { started_at: Instant::now(),
   closing: false, ... })`.
4. Schedule a background timer for `MOTION_MORPH + 60 ms`; clear the morph only
   if the epoch still matches and it is still the opening morph.

While an opening morph exists, the real viewer image stack is rendered at
opacity 0. When the timer clears the morph, the real image appears at the same
rectangle, so the handoff is invisible.

**Close.** In `close_quick_look()`, clone the current thumbnail/preview/color
and compute the start rectangle before clearing viewer state:

- normally `from = viewer_image_rect(...)`;
- if an opening morph is still in flight, `from` is the morph's *current*
  interpolated rectangle, not the full viewer rectangle. `ViewerMorph` stores
  `started_at`, so a small helper evaluates the same `ease_motion` curve and
  avoids a flash to full size.

Resolve the return target by the same source preference. If it is unavailable,
set `to = from` and `fade_only = true`. Set `viewer_morph` as a closing morph,
`quick_look_open = false`, and schedule the same guarded cleanup. Closing focus
behavior is unchanged; the grid is interactive immediately.

**Other interactions during the opening morph.** Next/previous and the details
toggle cancel the opening morph immediately and show the real current image
before applying their own animation. This prevents the overlay from continuing
to show the previous photo or a stale rectangle. The epoch guard makes the old
timer a no-op. Closing instead uses the in-flight rectangle described above,
because a cancel-then-close would flash the full-size image.

**Overlay.** `render()` appends `viewer_morph_overlay()` last, above the viewer
and all overlays, when `viewer_morph` is `Some`:

- frame: absolute, `overflow_hidden`, photo color background, interpolated
  `left/top/width/height` and radius (card 12 px <-> viewer 10 px) over
  `MOTION_MORPH`, keyed by `(viewer_morph_epoch, closing)`;
- opening: old layer = thumbnail with `Cover`, opacity 1 -> 0; new layer =
  preview (or thumbnail) with `Contain`, opacity 0 -> 1;
- closing: old layer = preview/thumbnail with `Contain`, opacity 1 -> 0;
  new layer = thumbnail with `Cover`, opacity 0 -> 1;
- `fade_only`: only the current image at `from`, fading out;
- closing backdrop: full-window dark + photo tint, opacity 1 -> 0, so the grid
  is revealed smoothly; opening relies on the viewer's own backdrop fade.

Both image layers carry their own interpolated rounded radius because GPUI's
overflow clipping is rectangular (the pattern already used for card/detail
images).

### 5.7 Viewer navigation, aspect changes, and details

Add `viewer_nav_epoch: u64` and `viewer_nav_direction: f32` (`+1` for next,
`-1` for previous). `step_viewer_photo()` updates them when the index changes.

Also add a single image-box animation state so aspect changes and details
toggles share one timeline:

```
struct ViewerBoxState {
    epoch: u64,
    started_at: Instant,
    from_aspect: f32,
    to_aspect: f32,
    from_details: bool,
    to_details: bool,
    duration: Duration,
}
```

- Opening the viewer resets the epoch and uses equal from/to values.
- Next/previous records the old photo's aspect and the new photo's aspect,
  duration `MOTION_BASE`.
- Toggling details records the old and new details booleans with the current
  aspect, duration `MOTION_SLOW`.
- A helper computes the from/to boxes with `viewer_image_box()` + centering and
  interpolates width/height with `ease_motion`; the frame is flex-centered, so
  its center tracks the details panel automatically.
- A `current_box()` helper evaluates the same timeline at `Instant::now()`.
  Closing uses it, so closing during an aspect/details animation does not snap
  to the target box.

The viewer image area becomes a relative frame sized by that animation. Inside
it:

- previous preview (when `previous_preview` is still valid) is an absolute
  layer that fades out and slides `-direction * VIEWER_NAV_SLIDE`;
- current preview is an absolute layer that fades in and slides from
  `direction * VIEWER_NAV_SLIDE` to 0;
- both use `MOTION_BASE`, keyed by `viewer_nav_epoch`.

`previous_preview` is already maintained by `set_primary_photo()` and retained
for 260 ms, so the 220 ms transition fits. When it is absent, only the incoming
layer animates. This replaces the current per-photo opacity fade.

The viewer top bar gets a small downward slide + fade over `MOTION_BASE` keyed
by `quick_look_epoch`, layered under the root fade.

The details body is wrapped in a `MOTION_FAST` fade + 6 px slide keyed by
`quick_look_details_epoch`. It mounts only while open, so closing still removes
the content immediately while the panel collapses; the width animation carries
the eye.

### 5.8 Micro-interactions

- **Manual compression dialog**: scrim fades over `MOTION_FAST`; panel fades
  and rises 10 px over `MOTION_BASE`. Both use constant element IDs and mount
  with the dialog, so every open replays.
- **Import report and global alerts**: fade + 8 px downward slide over
  `MOTION_BASE` when mounted.
- **Multi-select bar**: fixed 36 px height animated 0 -> 36 with opacity over
  `MOTION_FAST`, `overflow_hidden`. Exit remains immediate.
- **Compression profile selection**: the right editor cross-fades and rises
  4 px keyed by `profile_index`; profile cards animate their background and
  border via keys `(profile.id, active)` over `MOTION_FAST`.
- **Card selection badge**: the check badge gets a `MOTION_FAST` fade + 3 px
  settle when it mounts. Selected-card border/glow stays immediate; an
  animated ring is a possible later refinement.
- **Nav tooltip** as described in 5.2.

All animations use `.with_max_fps(MOTION_FPS)` where they run longer than a
few frames. `with_animation` handles reduce-motion automatically; the morph
creation and its timer are skipped when `cx.reduce_motion()` is true.

### 5.9 Z-order and hit testing

- The morph overlay is the last child of the root, above the viewer, manual
  compression overlay, nav tooltip, and rail toggle.
- It has no click handlers and no `occlude()`, so it does not consume input.
- The closing backdrop is also non-interactive; clicks during the 340 ms close
  can reach the grid. If the viewer is reopened during that window, the new
  morph replaces the old one and the epoch guard prevents the old timer from
  clearing it.
- Existing focus handling in `open_quick_look()` / `close_quick_look()` is not
  changed.

## 6. Testing

### Unit tests

- `nav_index` matches sidebar order and `nav_item_top` spacing.
- `screen_direction` returns +1 moving down, -1 moving up.
- `lerp_bounds` hits both endpoints and the midpoint.
- `viewer_image_rect` is centered, inside padding, and shifts left by half the
  details width; its size matches `viewer_image_box`.
- `ViewerBoxState` hits both boxes at t=0/1 and interpolates mixed portrait /
  landscape aspects at the midpoint.
- `ViewerMorph::current_bounds` at t=0/0.5/1 matches `lerp_bounds`.
- Source preference resolves card -> inspector and inspector -> card fallbacks.
- Motion constants are ordered fast < base < slow < morph.
- Existing `viewer_image_box`, `ease_motion`, sidebar, and selection tests keep
  passing.

### Manual verification (`--demo`)

1. Switch through all five sidebar tabs and back; confirm the pill and blue bar
   travel together, labels/icons cross-fade, and content enters from the
   correct side.
2. Collapse and expand the sidebar on several tabs; confirm the bar follows,
   the icon slides to the center, and no element snaps mid-flight.
3. Double-click a card: confirm the image grows from the card into the viewer,
   the thumbnail cross-fades to the full preview, and there is no final jump.
4. Press Esc: confirm the image returns to the same card. Repeat after
   next/previous, including a card scrolled out of view (fade fallback).
5. Open from the inspector's "全画面表示" button after scrolling the card out of
   view: confirm the morph starts from the inspector preview and returns there.
6. Step through photos with arrow keys/buttons: confirm opposite-direction
   cross-fades, a smoothly resizing frame on portrait/landscape changes, and a
   stable details panel.
7. Toggle details open/closed during the viewer: confirm the image resizes in
   step with the panel instead of jumping.
8. Trigger next/previous, details, and Esc during the first ~340 ms after
   opening: confirm there is no stale image and no full-size flash.
9. Open the manual-compression dialog, trigger an import report, multi-select,
   and hover collapsed nav icons: confirm each enters smoothly.
10. Turn on reduce motion; confirm every transition becomes an immediate state
    change, including viewer open/close.
11. Narrow and wide window sizes; confirm morph endpoints still align and the
    viewer image stays inside its padding.

### Local commands

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 7. Risks and accepted trade-offs

- **Rapid tab switching** can restart the indicator from the previous screen
  rather than its in-flight position. The 220 ms duration makes this rare; a
  full in-flight position tracker is not worth the state.
- **Mid-morph viewport/resize** uses the rectangle computed at the start. The
  overlay is short-lived; no re-targeting.
- **Cross-fading cover and contain images** can show slight letterboxing of the
  incoming image during flight. The frame background uses the photo's dominant
  color to blend it, and both endpoints are exact.
- **Off-screen close targets** deliberately fall back to a fade rather than
  flying to a stale rectangle.
- **The old screen is not cross-faded.** The new screen's enter transition is
  the continuity cue; retaining two screen trees would duplicate state.

## 8. Left to the builder

- Exact opacity curve for the thumbnail/preview cross-fade (straight linear
  `t` vs. a short hold at each end).
- Whether the screen enter starts at opacity 0 or 0.4 after visual review.
- Exact nav icon padding values if 18.5 px does not look optically centered.
- Whether the selected-card border also deserves an animated ring after the
  first visual pass.
- Whether the year-rail hover expansion should be added as a follow-up.
