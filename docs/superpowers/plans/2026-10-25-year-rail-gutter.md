# Year Rail Gutter and Compact Marker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the library and search year rail a dedicated 20 px strip for the white track, show only a small black marker at rest, and restrict click/drag starts to the track and current-position pill without breaking pill dragging.

**Post-review adjustment:** The originally planned 96 px gutter was reduced to 20 px after visual review. The pill and year labels now overlay cards when visible. A press additionally requires the gutter or pill hitbox to be hovered so an occluding overlay (Quick Look) cannot route clicks into the hidden grid. The 96 px values in the historical task steps below are superseded by this note.

**Architecture:** Keep the custom `YearRail` element and `UniformListScrollHandle`. Reserve the gutter with right padding on grid rows, add hover state to `PhotoBrowser`, and split the single full-rail hitbox into a wheel-pass-through gutter hitbox plus narrow track and pill hitboxes. Pure geometry helpers carry the hit-test rules and are unit tested.

**Tech Stack:** Rust 2024, gpui-kit 0.8 (`gpui-pre` elements and hitboxes), existing `cargo test` suite in `crates/ui/src/main.rs`.

**Spec:** `docs/superpowers/specs/2026-10-25-year-rail-gutter-design.md`

## Global Constraints

- Only change `crates/ui/src/main.rs`; do not add dependencies or touch year-bucket generation, scroll mapping, page loading, or card design.
- `RAIL_WIDTH = 96.0`, `RAIL_MARKER_SIZE = 10.0`, `RAIL_HIT_WIDTH = 12.0`; the month pill stays `80 x 18 px` with a 4 px right margin.
- The library and search grids must behave identically.
- Preserve wheel scrolling over the gutter and continue a drag after the pointer leaves the rail.
- Commit prefixes: `feat:`, `fix:`, `docs:`, `chore:`. Do not mix the design/plan documents into the code commits; they go through a separate docs branch/PR.
- Final verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

## Review Focus

- **Wheel over the reserved gutter:** the list must still scroll because the rail is no longer over cards. Manual check in Task 3.
- **Pill dragged past the top/bottom or outside the rail:** `jump_to` must keep following the pointer and clamp at the ends. Manual check in Task 3.
- **Press on the collapsed marker:** the marker sits on the track, so the 12 px track strip must still start a drag. Unit test in Task 1.
- **Fast hover then click on the pill:** the pill must be expanded before its wider rectangle accepts a press; a press outside the track while collapsed does nothing. Unit test in Task 1, manual check in Task 3.
- **Empty library or a viewport shorter than the rail padding:** no rail is mounted for zero photos, and short rails must not panic or produce a negative hit rectangle. Existing `rail_y` tests cover the short-height math; manual check in Task 3.

---

## Task 1: Rail geometry and press hit-testing

**Files:**
- Modify: `crates/ui/src/main.rs` (constants near `RAIL_WIDTH`, helpers near `rail_y`, test imports and `mod tests`)
- Test: `crates/ui/src/main.rs` (tests module)

**Interfaces:**
- Consumes: gpui `Bounds<Pixels>`, `Point<Pixels>`, `point`, `px`, `size`.
- Produces:
  - `const RAIL_WIDTH: f32 = 96.0;`
  - `const RAIL_PILL_WIDTH: f32 = 80.0;`
  - `const RAIL_PILL_RIGHT_MARGIN: f32 = 4.0;`
  - `const RAIL_MARKER_SIZE: f32 = 10.0;`
  - `const RAIL_HIT_WIDTH: f32 = 12.0;`
  - `fn rail_track_hit_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels>`
  - `fn rail_pill_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels>`
  - `fn rail_marker_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels>`
  - `fn rail_press_hits(position: Point<Pixels>, bounds: Bounds<Pixels>, pill_y: f32, expanded: bool) -> bool`

- [ ] **Step 1: Write the failing geometry tests**

In `mod tests`, add `rail_marker_bounds`, `rail_pill_bounds`, `rail_press_hits`, `rail_track_hit_bounds`, `RAIL_MARKER_SIZE`, `RAIL_PILL_WIDTH`, and `RAIL_WIDTH` to the `use super::{...}` list. Add `Bounds` and `size` to the `gpui_kit` import. Then add:

```rust
#[test]
fn rail_geometry_limits_presses_to_track_and_expanded_pill() {
    let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
    let pill_y = 300.0;

    // Track strip: x 88..100, y 44..596 for this rail.
    assert!(rail_press_hits(point(px(94.), px(100.)), bounds, pill_y, false));
    // Expanded pill: x 22..102, y 311..329.
    assert!(rail_press_hits(point(px(50.), px(320.)), bounds, pill_y, true));
    // The same point is inert while the pill is collapsed.
    assert!(!rail_press_hits(point(px(50.), px(320.)), bounds, pill_y, false));
    // Blank gutter left of the pill never jumps.
    assert!(!rail_press_hits(point(px(15.), px(320.)), bounds, pill_y, true));
    // Top and bottom padding of the track are outside the interactive strip.
    assert!(!rail_press_hits(point(px(94.), px(30.)), bounds, pill_y, true));
    assert!(!rail_press_hits(point(px(94.), px(610.)), bounds, pill_y, true));
}

#[test]
fn rail_marker_sits_on_the_track_at_the_pill_center() {
    let bounds = Bounds::new(point(px(10.), px(20.)), size(px(RAIL_WIDTH), px(600.)));
    let pill_y = 300.0;
    let marker = rail_marker_bounds(bounds, pill_y);
    let pill = rail_pill_bounds(bounds, pill_y);

    assert_eq!(f32::from(marker.size.width), RAIL_MARKER_SIZE);
    assert_eq!(f32::from(marker.size.height), RAIL_MARKER_SIZE);
    assert_eq!(f32::from(pill.size.width), RAIL_PILL_WIDTH);
    assert_eq!(marker.center().y, pill.center().y);
    assert!(rail_track_hit_bounds(bounds).contains(&marker.center()));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui rail_geometry_limits_presses_to_track_and_expanded_pill`

Expected: FAIL with `cannot find function 'rail_press_hits'` (and the other missing helpers).

- [ ] **Step 3: Implement the constants and helpers**

Add the constants beside the existing rail constants. Implement in `crates/ui/src/main.rs`:

```rust
/// Hit strip centered on the 2 px rail track.
fn rail_track_hit_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let track_x = bounds.right() - px(RAIL_TRACK_INSET);
    let usable = (f32::from(bounds.size.height) - 2.0 * RAIL_TOP_PAD).max(0.0);
    Bounds::new(
        point(track_x - px(RAIL_HIT_WIDTH / 2.0), bounds.top() + px(RAIL_TOP_PAD)),
        size(px(RAIL_HIT_WIDTH), px(usable)),
    )
}

/// The expanded month pill, right-aligned inside the rail.
fn rail_pill_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels> {
    let center_y = bounds.top() + px(pill_y);
    Bounds::new(
        point(
            bounds.right() - px(RAIL_PILL_WIDTH + RAIL_PILL_RIGHT_MARGIN),
            center_y - px(RAIL_PILL_HEIGHT / 2.0),
        ),
        size(px(RAIL_PILL_WIDTH), px(RAIL_PILL_HEIGHT)),
    )
}

/// The compact black marker shown at rest, centered on the track.
fn rail_marker_bounds(bounds: Bounds<Pixels>, pill_y: f32) -> Bounds<Pixels> {
    let center = point(
        bounds.right() - px(RAIL_TRACK_INSET),
        bounds.top() + px(pill_y),
    );
    Bounds::new(
        point(
            center.x - px(RAIL_MARKER_SIZE / 2.0),
            center.y - px(RAIL_MARKER_SIZE / 2.0),
        ),
        size(px(RAIL_MARKER_SIZE), px(RAIL_MARKER_SIZE)),
    )
}

/// A press may start a rail drag only on the track or, while expanded, the pill.
fn rail_press_hits(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    pill_y: f32,
    expanded: bool,
) -> bool {
    rail_track_hit_bounds(bounds).contains(&position)
        || (expanded && rail_pill_bounds(bounds, pill_y).contains(&position))
}
```

Use the existing `RAIL_TOP_PAD`, `RAIL_TRACK_INSET`, and `RAIL_PILL_HEIGHT` values.

- [ ] **Step 4: Run the geometry tests to verify they pass**

Run: `cargo test -p vrc-photo-ui rail_`

Expected: PASS, including the two new geometry tests.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add year rail hit geometry"
```

---

## Task 2: Reserve the dedicated rail gutter

**Files:**
- Modify: `crates/ui/src/main.rs` (`grid_available_width`, `search_available_width`, the row builder in `photo_grid`, and column tests)

**Interfaces:**
- Consumes: `RAIL_WIDTH` from Task 1.
- Produces: no new functions. `grid_available_width` and `search_available_width` now return the width left for cards after the gutter, and each rendered row reserves `RAIL_WIDTH` at its right edge.

- [ ] **Step 1: Write the failing layout expectations**

Update `grid_columns_adapt_to_window_rail_and_inspector` to the post-gutter counts:

```rust
#[test]
fn grid_columns_adapt_to_window_rail_and_inspector() {
    assert_eq!(grid_columns(grid_available_width(1280.0, false, false)), 3);
    assert_eq!(grid_columns(grid_available_width(1440.0, false, false)), 4);
    assert_eq!(grid_columns(grid_available_width(1280.0, true, false)), 4);
    assert_eq!(grid_columns(grid_available_width(1440.0, true, false)), 4);
    assert_eq!(grid_columns(grid_available_width(1280.0, false, true)), 4);
    assert_eq!(grid_columns(grid_available_width(1440.0, false, true)), 5);
    assert_eq!(grid_columns(grid_available_width(1080.0, false, false)), 2);
    assert_eq!(grid_columns(grid_available_width(1080.0, false, true)), 3);
    assert_eq!(grid_columns(0.0), 1);
}
```

Add a width assertion to the same test or a small new test:

```rust
assert_eq!(grid_available_width(1440.0, true, false), 892.0);
assert_eq!(search_available_width(1280.0, false), 578.0);
```

- [ ] **Step 2: Run the layout tests to verify they fail**

Run: `cargo test -p vrc-photo-ui grid_columns_adapt_to_window_rail_and_inspector`

Expected: FAIL; the current widths still produce 5/5/6/4 columns and 988/674.

- [ ] **Step 3: Subtract the gutter in both width helpers**

In `grid_available_width` and `search_available_width`, subtract `RAIL_WIDTH` before `max(0.0)`:

```rust
(window_width - sidebar_width(sidebar_collapsed) - inspector - WORKSPACE_PAD_X - RAIL_WIDTH).max(0.0)
```

```rust
(window_width - sidebar_width(sidebar_collapsed) - 290.0 - 26.0 - 52.0 - RAIL_WIDTH).max(0.0)
```

Update the doc comments to say the returned width is the card area after the rail gutter.

- [ ] **Step 4: Reserve the gutter inside every rendered row**

In the `uniform_list` row builder in `photo_grid`, add `.pr(px(RAIL_WIDTH))` to the `h_flex()` row. Keep the list element full width so its scroll hitbox remains under the gutter and wheel events still reach it. The row's cards keep `flex_1`, so they stop before the rail.

- [ ] **Step 5: Run the UI tests**

Run: `cargo test -p vrc-photo-ui`

Expected: PASS, including the updated column counts and the unchanged search-width tests.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: reserve a gutter for the year rail"
```

---

## Task 3: Compact marker, hover expansion, and narrow drag surface

**Files:**
- Modify: `crates/ui/src/main.rs` (`PhotoBrowser`, `YearRail`, `YearRail::new`, `Element::prepaint`, `Element::paint`, `photo_grid` rail construction)

**Interfaces:**
- Consumes: `rail_track_hit_bounds`, `rail_pill_bounds`, `rail_marker_bounds`, `rail_press_hits` from Task 1; `RAIL_WIDTH` from Task 2.
- Produces:
  - `PhotoBrowser.rail_hover: Rc<Cell<bool>>`
  - `YearRail.hovered: Rc<Cell<bool>>`
  - `YearRail::new(..., drag_active: Rc<Cell<bool>>, hovered: Rc<Cell<bool>>)`
  - `struct YearRailPrepaint { track: Hitbox, pill: Option<Hitbox> }`

- [ ] **Step 1: Add the shared hover state**

In `PhotoBrowser`, add `rail_hover: Rc<Cell<bool>>` beside `rail_drag`; initialize it with `Rc::new(Cell::new(false))`, and set it to `false` in `replace_index`.

In `YearRail`, add `hovered: Rc<Cell<bool>>`; extend `YearRail::new` with the final `hovered` parameter and pass `browser.rail_hover.clone()` from `photo_grid`.

- [ ] **Step 2: Replace the prepaint hitbox with layered hitboxes**

Change `type PrepaintState` to `YearRailPrepaint`:

```rust
struct YearRailPrepaint {
    track: Hitbox,
    pill: Option<Hitbox>,
}
```

In `prepaint`, compute `height` and the current `pill_y` the same way `paint` does (`first_visible_photo(list_scroll_top(&self.scroll), columns, total)` then `rail_y`). Insert:

1. `window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll)` for wheel pass-through and to block cards under the gutter;
2. `window.insert_hitbox(rail_track_hit_bounds(bounds), HitboxBehavior::Normal)`;
3. only when `self.hovered.get() || self.drag_active.get()`, `window.insert_hitbox(rail_pill_bounds(bounds, pill_y), HitboxBehavior::Normal)`.

Return the track and optional pill hitboxes. Later-inserted hitboxes are in front, so the narrow track/pill remain hoverable on top of the blocking gutter.

- [ ] **Step 3: Paint the compact marker or the expanded pill**

In `paint`, compute `let expanded = self.hovered.get() || self.drag_active.get();`.

- When `!expanded`, paint only the marker from `rail_marker_bounds(bounds, pill_y)`: a black near-opaque fill (`rgba(0x05070af2)`, 3 px corner radius) with a 1 px `rgba(0xffffff2e)` ring for contrast. Do not paint the month text.
- When `expanded`, paint the existing pill using `rail_pill_bounds(bounds, pill_y)` and the cached month line. Keep the existing `rail_label_hidden_by_pill(y, pill_y)` rule.
- Keep the 2 px track and year labels unchanged.
- Set `CursorStyle::PointingHand` only on the track hitbox and, when present, the pill hitbox. Do not set it on the whole gutter.

- [ ] **Step 4: Restrict mouse-down and keep drag tracking**

In the `MouseDownEvent` listener:

```rust
let first_visible = first_visible_photo(list_scroll_top(&rail.scroll), rail.columns.max(1), total);
let pill_y = rail_y(first_visible, total, height);
let expanded = rail.hovered.get() || rail.drag_active.get();
if !rail_press_hits(event.position, bounds, pill_y, expanded) {
    return;
}
```

Only after that check should the handler call `cx.stop_propagation()`, set `hovered` and `drag_active`, and call `jump_to`. Remove the old `hitbox.is_hovered(window)` gate. Blank gutter and year-label presses must return without jumping or starting a drag.

In the `MouseMoveEvent` listener:

- While `drag_active` is set, keep the current behavior: keep calling `jump_to` on every dragging move regardless of pointer location; clear the drag and refresh if `event.dragging()` is false.
- While not dragging, compute `bounds.contains(&event.position)` and update `hovered` only when it changes; call `window.refresh()` on that change.

In the `MouseUpEvent` listener, clear `drag_active`, set `hovered` from `bounds.contains(&event.position)`, and call `window.refresh()` when the drag ended or the hover state changed.

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -p vrc-photo-ui`

Expected: PASS; existing rail, layout, and anchor tests stay green.

- [ ] **Step 6: Manually verify the interaction in the demo app**

Run: `cargo run -p vrc-photo-ui -- --demo`

Check on both the library and search tabs:

- at rest, only the 10 px black marker is visible at the current row;
- moving into the gutter expands the month pill; moving out collapses it;
- clicking the track jumps; dragging along the track scrolls;
- clicking a year label or blank gutter does not jump;
- grabbing the pill and dragging outside the rail keeps following the pointer and clamps at the top/bottom;
- the mouse wheel over the gutter still scrolls the grid;
- no card appears under the gutter.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: collapse the year rail marker at rest"
```

---

## Task 4: Full verification and branch handoff

**Files:**
- Verify: `crates/ui/src/main.rs`

- [ ] **Step 1: Run formatting**

Run: `cargo fmt --all -- --check`

Expected: exit 0, no diff.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: exit 0, no warnings.

- [ ] **Step 3: Run the workspace tests**

Run: `cargo test --workspace`

Expected: all tests pass.

- [ ] **Step 4: Re-run the manual demo pass for both grids**

Run: `cargo run -p vrc-photo-ui -- --demo`

Repeat the Task 3 checks at a wide window and at the 1080 px minimum width. Confirm the column count follows the updated tests and the gutter never overlaps a card.

- [ ] **Step 5: Commit any verification fixes**

If verification required changes:

```bash
git add crates/ui/src/main.rs
git commit -m "fix: address year rail verification findings"
```

If no changes were needed, do not create an empty commit.
