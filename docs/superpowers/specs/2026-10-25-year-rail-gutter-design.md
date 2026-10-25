# Year Rail Gutter and Compact Marker: Design

- Date: 2026-10-25
- Status: Approved
- Target: `crates/ui/src/main.rs`

## Post-review adjustment (2026-10-25)

After visual review the user asked for a narrower dedicated strip: only the
white track plus a little margin. The final implementation supersedes the
96 px values below as follows:

- `RAIL_WIDTH` is **20 px**, not 96 px. Only this strip is reserved from the
  cards; the month pill and year labels paint outside it over the cards when
  visible.
- Hover starts in the narrow gutter. Once the pill is expanded, hovering the
  pill outside the gutter keeps it open so it can be grabbed.
- A press requires the gutter hitbox or pill hitbox to be hovered, which also
  prevents clicks from passing through the Quick Look overlay.
- Column counts therefore return to their pre-gutter values (5/5/6/4 for the
  tested sidebar/inspector combinations) minus only the 20 px strip.

## 1. Intent

The library and search grids share a Google Photos-style year rail painted
over the right edge of the grid. It is also the scrollbar: a 2 px track, year
labels, and a black month pill at the current position. The whole 88 px rail
currently accepts clicks and blocks the cards underneath.

The user wants:

- a little more dedicated space for the scrollbar;
- only a small black marker when the pointer is not over the rail;
- click/drag hit-testing limited to the white track and the current-position
  pill, not the whole rail; and
- grabbing the black pill and dragging to keep working.

The marker must never get in the way of the drag that starts on it.

## 2. Current behavior

- `YearRail` fills an 88 px strip at the right edge of each grid, on top of
  the list.
- `RAIL_TRACK_INSET = 12`, so the 2 px track sits 12 px from the right edge.
- The month pill is 80 x 18 px, right-aligned 4 px from the edge, and shows
  `YYYY年M月`.
- A single `BlockMouseExceptScroll` hitbox covers the whole rail. A left press
  anywhere in it starts `jump_to` and sets `drag_active`; moves keep jumping
  even after the pointer leaves the rail.
- Year labels are right-aligned and are not separately interactive; they only
  appear clickable because the whole rail is.

## 3. Goals and non-goals

Goals:

- Reserve a 96 px dedicated right gutter in both grids. Cards stop before it;
  the scrollbar never sits on top of a card.
- Wheel scrolling must keep working while the pointer is over the gutter.
- At rest, paint only a 10 px black rounded marker at the current row.
- While the pointer is over the gutter or a drag is active, expand the marker
  back to the existing 80 x 18 px month pill with its text.
- Start a click/drag only on the track strip (12 px wide around the 2 px line)
  or the expanded pill. Blank gutter and year labels do not jump.
- Dragging from the pill continues to follow the pointer outside the gutter.
- Apply the same behavior to the library and search grids.

Non-goals:

- Changing year bucket generation, label spacing, rail-to-row mapping, or page
  loading.
- Replacing `UniformListScrollHandle` or the custom rail with another
  scrollbar.
- Adding an expand/collapse animation.
- Changing card sizes or the grid's visual design beyond the right gutter.

## 4. Approaches considered

### A. Keep the custom `YearRail` and split layout and hit-testing (chosen)

Keep the existing element, scroll handle, `jump_to`, and drag state. Move the
rail's space out of the list content with right padding on grid rows, and add
narrow hitboxes plus a hover flag. This is the smallest change that preserves
the proven scroll and drag behavior.

### B. Rewrite the rail as GPUI divs

Render the track, marker, and pill as divs with `.on_hover()` and mouse
handlers, leaving only the year labels custom-painted. This makes hover
styling natural, but it reimplements absolute placement, hit-testing, and
pointer tracking during a drag. The regression risk is higher and the visual
result is the same.

## 5. Design

### 5.1 Constants and layout

- `RAIL_WIDTH`: 88 -> 96 px.
- `RAIL_TRACK_INSET`: unchanged at 12 px.
- New `RAIL_HIT_WIDTH`: 12 px, centered on the track.
- New `RAIL_MARKER_SIZE`: 10 px.
- Pill geometry unchanged: 80 x 18 px, right margin 4 px, 9 px corner radius,
  10 px text.

Grid rows get `pr(px(RAIL_WIDTH))`. The list element itself stays full width,
so its scroll hitbox remains under the gutter and wheel events still scroll
the list. The rail stays absolutely positioned in the right 96 px.

`grid_available_width` and `search_available_width` subtract `RAIL_WIDTH` so
the column count is computed from the actual card area. At some window widths
the count can drop by one column; that is the intended cost of a dedicated
gutter.

### 5.2 Hover and paint state

`PhotoBrowser` gains `rail_hover: Rc<Cell<bool>>`, next to `rail_drag`.

- Expanded mode is `rail_hover || rail_drag`.
- A mouse-move listener updates `rail_hover` when the pointer enters or
  leaves the rail bounds and calls `window.refresh()` only on a change.
- A button-up or lost-drag check clears `rail_drag` and recomputes
  `rail_hover` from the pointer position.
- When collapsed, paint a 10 x 10 black rounded marker centered on the track
  at the current row. Do not paint the month text.
- When expanded, paint the current 80 x 18 px pill and month text. Keep the
  existing rule that hides a year label covered by the pill.

### 5.3 Hit-testing and drag

The rail inserts two layers:

1. a full-gutter `BlockMouseExceptScroll` hitbox for wheel pass-through and to
   keep cards from hovering underneath; and
2. `Normal` hitboxes for the 12 px track strip and the expanded pill, used for
   the pointing-hand cursor.

Mouse-down behavior:

- If the point is inside the track strip or the expanded pill, stop
  propagation, set `rail_hover` and `rail_drag`, and call `jump_to` as today.
- Otherwise do nothing inside the rail; blank gutter and year labels do not
  scroll the grid.
- The compact marker sits on the track, so the track strip is its grab area.

The existing drag listener stays semantically the same: once `rail_drag` is
set, every dragging mouse-move calls `jump_to` regardless of the pointer's
position. This is what keeps a grabbed pill usable after the pointer leaves
the gutter. Only button-up or a move with no button held ends the drag.

### 5.4 Geometry helpers

Extract pure helpers so the pill, marker, track, and hit rectangles can be
unit tested without GPUI:

- current pill bounds from the first visible row and rail height;
- compact marker bounds from the same row;
- track strip bounds;
- `rail_hit_test(position, ...)` returning whether a press may start a drag.

## 6. Testing

- Unit-test the geometry helpers and the hit test: track yes, pill yes,
  year-label area no, blank gutter no.
- Update the column-count assertions for the reduced available width.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, and `cargo test --workspace`.
- Manual pass in `--demo`: library and search tabs, hover in/out, click the
  track, grab and drag the pill outside the gutter, wheel over the gutter,
  and wide/narrow window widths.

## 7. Left to the builder

- Exact marker shade and whether it needs a subtle outline for contrast.
- Exact hover slop around the pill if a few pixels prove hard to grab.
- Whether the hover state must be reset when switching tabs; a stale expanded
  state is corrected by the next mouse move.
