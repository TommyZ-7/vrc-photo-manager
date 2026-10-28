# UI notifications and chrome cleanup — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the top-of-content notification surfaces with bottom-right toasts plus a bell-driven notification modal, remove the duplicated header/sidebar status, and fix the sidebar tooltip, library row gap, and viewer switch animation.

**Architecture:** Transient feedback goes through the existing `gpui-component` `NotificationList`. Action-required state (import report, library error, compression/daemon failures) is rendered by a new custom `ui/notifications.rs` modal owned by `App`. The old `App.message`, ribbon status group, global alert bars, message strip, import panel, and library error row are removed. Pure helpers isolate the toast/failure transitions and tooltip/viewer geometry so they can be tested without a window.

**Tech Stack:** Rust 2024, gpui-kit 0.8.1 (`gpui-fast`), gpui-component 0.8.x, SQLite.

**Spec:** `docs/superpowers/specs/2026-10-28-ui-notifications-design.md`

## Global Constraints

- **Disable parallelism for every heavy command.** Use `cargo ... -j 1`; run tests with `-- --test-threads=1`. Never run two cargo builds/tests concurrently, and do not dispatch parallel subagents that compile.
- Run the full local gate before pushing:
  `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -j 1 -- -D warnings && cargo test --workspace -j 1 -- --test-threads=1`
- Do not mix product docs in this PR. `docs/ui.md` is a separate docs-only PR.
- Keep the existing dark panel colors, 22/17/13 px radii, and 14 px shell gaps.
- Keep `Ctrl/Cmd+B`, viewer shortcuts, and reduced-motion behavior working.
- No notification history persistence. The modal shows current action-required items only.
- Use `gpui-component` `NotificationList`; do not create a second toast lifecycle.

## Review Focus

- **A toast pushed during `App::new` before the component Root exists.** Expected: it is queued and appears after the first render, never panics.
- **A pre-existing compression/daemon failure at launch.** Expected: bell dot and modal entry, but no toast until the count increases after the baseline is seeded.
- **Import report with failures/remaining files.** Expected: summary, retry, resume, and collapsible details all still work from the modal.
- **Toasts arriving while the viewer is open.** Expected: toast is visible above the viewer; clicking an actionable one opens the modal above the viewer; closing returns to the viewer.
- **Collapsed sidebar tooltip with the menu at the top.** Expected: it appears to the right of the icon at the item's vertical position and is not clipped or pushed into flow.

---

### Task 1: Notification state, toasts, and startup queue

**Files:**
- Create: `crates/ui/src/ui/notifications.rs`
- Modify: `crates/ui/src/ui/mod.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/main.rs`
- Test: inline `#[cfg(test)]` in `app.rs` and `ui/notifications.rs`

**Interfaces:**
- Produces on `App`:
  - `window_handle: AnyWindowHandle`
  - `pending_toasts: Vec<ToastSpec>`
  - `toasts_ready: bool`
  - `toast(&mut self, tone: NotificationType, message: impl Into<SharedString>, cx: &mut Context<Self>)`
  - `toast_info/success/warning/error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>)`
  - `toast_actionable(&mut self, tone: NotificationType, message: impl Into<SharedString>, cx: &mut Context<Self>)`
  - `flush_pending_toasts(&mut self, window: &mut Window, cx: &mut Context<Self>)`
- Produces `pub(crate) struct ToastSpec { tone: NotificationType, message: SharedString, opens_notifications: bool }`.
- Produces `pub(crate) fn failure_transition(previous: Option<(i64, usize)>, next: (i64, usize)) -> Vec<FailureNotice>` with `FailureNotice { Compression { count }, Daemon { count } }`.
- `App::toast` queues when `!toasts_ready`, otherwise pushes through `window_handle.update`.
- `App::render` flushes the queue once via `cx.defer_in` after setting `toasts_ready = true`.

- [ ] **Step 1: Write the failing failure-transition test**

In `app.rs` tests, assert:
```rust
assert!(failure_transition(None, (3, 2)).is_empty());
assert_eq!(
    failure_transition(Some((3, 2)), (5, 2)),
    vec![FailureNotice::Compression { count: 5 }]
);
assert_eq!(
    failure_transition(Some((3, 2)), (3, 4)),
    vec![FailureNotice::Daemon { count: 4 }]
);
assert!(failure_transition(Some((5, 4)), (5, 4)).is_empty());
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test -p vrc-photo-manager-ui failure_transition -j 1 -- --test-threads=1`
Expected: FAIL because `failure_transition` / `FailureNotice` do not exist.

- [ ] **Step 3: Implement the notification module and state**

Add `ui/notifications.rs` with `ToastSpec`, `FailureNotice`, `failure_transition`, and the `impl App` toast helpers. Add `mod notifications;` to `ui/mod.rs`.

Add to `App`: `window_handle`, `pending_toasts`, `toasts_ready`. In `App::new`, capture `let window_handle = window.window_handle();` before constructing `Self` and set `toasts_ready: false`.

In `App::render`, before layout:
```rust
if !self.toasts_ready {
    self.toasts_ready = true;
    cx.defer_in(window, |this, window, cx| this.flush_pending_toasts(window, cx));
}
```

In `main.rs`, after `Theme::change`, set:
```rust
let theme = Theme::global_mut(cx);
theme.notification.placement = Anchor::BottomRight;
theme.notification.width = px(340.);
theme.notification.margins = Edges { top: px(16.), right: px(16.), bottom: px(16.), left: px(16.) };
```

- [ ] **Step 4: Run the test and verify it passes**

Run: `cargo test -p vrc-photo-manager-ui failure_transition -j 1 -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/notifications.rs crates/ui/src/ui/mod.rs crates/ui/src/app.rs crates/ui/src/main.rs
git commit -m "feat(ui): add toast infrastructure and notification state"
```

---

### Task 2: Notification modal, bell, and import summary/details

**Files:**
- Modify: `crates/ui/src/ui/notifications.rs`
- Modify: `crates/ui/src/ui/actions.rs`
- Modify: `crates/ui/src/main.rs`
- Modify: `crates/ui/src/ui/chrome.rs`
- Modify: `crates/ui/src/ui/import.rs`
- Modify: `crates/ui/src/app.rs`
- Test: inline `#[cfg(test)]` in `ui/notifications.rs`

**Interfaces:**
- Produces `NotificationSection` enum and `pub(crate) fn notification_sections(import: bool, library_error: bool, failed_jobs: i64, daemon_errors: usize) -> Vec<NotificationSection>`.
- Produces on `App`:
  - `open_notification_modal(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `close_notification_modal(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `notification_attention(&self) -> bool`
  - `notification_button(&mut self, cx: &mut Context<Self>) -> AnyElement`
  - `notification_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement`
- Adds `CloseNotifications` action and `Esc` binding in the `notification_modal` key context.
- Adds to `App`: `notification_modal_open`, `notification_modal_epoch`, `import_report_expanded`, `notification_return_focus`, `notification_close_focus`.
- Adds `any_overlay_open(..., notification_modal_open)`.

- [ ] **Step 1: Write the failing section-order test**

```rust
assert_eq!(
    notification_sections(true, true, 2, 3),
    vec![
        NotificationSection::Import,
        NotificationSection::Library,
        NotificationSection::Compression,
        NotificationSection::Daemon,
    ]
);
assert!(notification_sections(false, false, 0, 0).is_empty());
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test -p vrc-photo-manager-ui notification_sections -j 1 -- --test-threads=1`
Expected: FAIL because the enum/helper do not exist.

- [ ] **Step 3: Implement the modal and bell**

- Add the enum/helper.
- Add the App fields and methods.
- Render the modal in `App::render` after the existing overlays. Root: absolute full-window backdrop, `key_context("notification_modal")`, `.occlude()`, backdrop click closes. Card: max width 720 px, max height 80% of the viewport, panel colors, scrollable body, close button using a focus handle.
- Sections:
  - Import: summary badges, retry/resume/details/dismiss buttons, then `import_report_details` when expanded.
  - Library: error text + reload.
  - Compression: failure count + Dashboard + retry.
  - Daemon: error count + Dashboard.
  - Empty: `対応が必要な通知はありません`.
- Replace the titlebar bell div with `self.notification_button(cx)`, keeping the amber dot when `notification_attention()`.
- Add `CloseNotifications` to `actions.rs`, bind `escape` with context `notification_modal` in `main.rs`, and handle it on the App root.
- Add `notification_modal_open` to `any_overlay_open` and its test.
- Split `ui/import.rs`: keep a `pub(crate) fn import_report_details(&self, report: &ImportReportView) -> AnyElement` with the existing failure/partial/renamed/scan sections, and remove the old top-of-screen panel.

- [ ] **Step 4: Run the test and verify it passes**

Run: `cargo test -p vrc-photo-manager-ui notification_sections -j 1 -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/notifications.rs crates/ui/src/ui/actions.rs crates/ui/src/main.rs crates/ui/src/ui/chrome.rs crates/ui/src/ui/import.rs crates/ui/src/app.rs
git commit -m "feat(ui): add the bell-driven notification modal"
```

---

### Task 3: Migrate messages to toasts and remove top notification surfaces

**Files:**
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/*.rs`
- Modify: `crates/ui/src/update_view.rs`
- Modify: `crates/ui/src/ui/chrome.rs`
- Modify: `crates/ui/src/ui/shell.rs`
- Modify: `crates/ui/src/ui/library.rs`
- Modify: `crates/ui/src/ui/search.rs`
- Modify: `crates/ui/src/ui/viewer.rs`

**Interfaces:**
- Consumes the Task 1 toast helpers and Task 2 modal.
- Removes `App.message`, `active_ribbon_tab`, `RibbonTab`, `default_ribbon_tab`, `message_strip`, `global_alerts`, `alert_summaries`, `message_strip_visible`, `ribbon_status_visible`, `ribbon_selection_label`, `titlebar_sync_visible`, `titlebar_sync_label`, and `status_note`.
- Adds `App::note_library_error(&mut self, error: String, cx: &mut Context<Self>)`.
- Changes `reload_job_state(&mut self, cx: &mut Context<Self>)` and uses `failure_transition` with a `failure_baseline: Option<(i64, usize)>`.

- [ ] **Step 1: Write the failing library-error dedupe test**

Add a pure helper test:
```rust
assert!(should_notify_library_error(None, "boom"));
assert!(!should_notify_library_error(Some("boom"), "boom"));
assert!(should_notify_library_error(Some("boom"), "different"));
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test -p vrc-photo-manager-ui should_notify_library_error -j 1 -- --test-threads=1`
Expected: FAIL because the helper does not exist.

- [ ] **Step 3: Replace message writes and remove the old surfaces**

- Replace every `self.message = Some(...)` / `this.message = Some(...)` with the appropriate `toast_*` call. Classify: saves/trash/retry/profile changes → success; progress/state → info; recoverable warnings → warning; failures/rejections → error.
- Replace `self.message = None` clears with no-op removal.
- `App::new`: convert `open_database`/empty-library/update-notice/daemon-launch messages to `toast_*` calls (queued).
- `reload_job_state`: seed `failure_baseline` on the first read; on later increases push actionable error toasts. Use `failure_transition`.
- On import report completion: set `import_report_expanded = false`, push an actionable toast (`success` when no failures/remaining, `warning` otherwise) using `import_summary`.
- On library error: set via `note_library_error`, which pushes an actionable error toast only when the message changes. On success clear it.
- Remove the `message` rendering from `chrome.rs`, `shell.rs`, `search.rs`, and `viewer.rs`. Remove the shell's alert/message-strip methods and the library's top import/error rows. Keep the library empty-state error card.
- Remove the `active_ribbon_tab` field and `RibbonTab` plumbing from `app.rs`, `lifecycle.rs`, `chrome.rs`.
- Make the sidebar import status `結果を見る` open the notification modal.
- Update `update_view.rs` messages to toasts; keep the update card's own phase/notice display.

- [ ] **Step 4: Run the test and verify it passes, then compile**

Run: `cargo test -p vrc-photo-manager-ui should_notify_library_error -j 1 -- --test-threads=1`
Run: `cargo check --workspace -j 1`
Expected: PASS / no errors.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src
git commit -m "refactor(ui): route feedback through toasts and remove top banners"
```

---

### Task 4: Header, sidebar, and collapsed-sidebar tooltip

**Files:**
- Modify: `crates/ui/src/ui/chrome.rs`
- Modify: `crates/ui/src/ui/shell.rs`
- Modify: `crates/ui/src/app.rs`
- Test: inline `#[cfg(test)]` in `ui/shell.rs` and `ui/chrome.rs`

**Interfaces:**
- Removes the daemon status pill and sync label from `titlebar`.
- Removes the 34 px toggle row and 12 px spacer from `sidebar`.
- Adds `pub(crate) fn nav_tooltip_origin(index: usize) -> (f32, f32)`:
  - `(WORKSPACE_PAD + sidebar_width(true) + 8.0, WORKSPACE_PAD + 16.0 + nav_item_top(index) + 8.5)`
- Adds `pub(crate) fn rail_toggle_origin(sidebar_collapsed: bool) -> (f32, f32)`:
  - `(WORKSPACE_PAD + sidebar_width(sidebar_collapsed) - 12.0, WORKSPACE_PAD + 18.0)`
- Tooltip animation keeps the element absolute and animates `left` only.

- [ ] **Step 1: Write the failing geometry tests**

```rust
assert_eq!(nav_tooltip_origin(0), (100.0, 38.5));
assert_eq!(nav_tooltip_origin(1), (100.0, 81.5));
assert_eq!(rail_toggle_origin(false), (240.0, 32.0));
assert_eq!(rail_toggle_origin(true), (80.0, 32.0));
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test -p vrc-photo-manager-ui nav_tooltip_origin rail_toggle_origin -j 1 -- --test-threads=1`
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Implement the layout changes**

- Remove the header status pill/sync label and their helpers/tests.
- Remove the sidebar toggle row/spacer. Start the nav list at the top and add `pr(px(18.))` so the floating toggle does not overlap the first item's count.
- Add the absolute `rail_toggle` after the main column, using `rail_toggle_origin`.
- Remove the bottom daemon dot row.
- Rewrite `nav_tooltip` to use `nav_tooltip_origin` and keep it absolute. The animation must not call `.relative()`; animate `left` from `target - 4.0` to `target`.
- Keep the tooltip as the last child of the shell so it paints above the main column.

- [ ] **Step 4: Run the tests and verify they pass**

Run: `cargo test -p vrc-photo-manager-ui nav_tooltip_origin -j 1 -- --test-threads=1`
Run: `cargo test -p vrc-photo-manager-ui rail_toggle_origin -j 1 -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/chrome.rs crates/ui/src/ui/shell.rs crates/ui/src/app.rs
git commit -m "fix(ui): restore the floating sidebar toggle and fix the tooltip overlay"
```

---

### Task 5: Library gap and viewer entrance animation

**Files:**
- Modify: `crates/ui/src/ui/theme.rs`
- Modify: `crates/ui/src/ui/viewer.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/viewer.rs`
- Test: inline `#[cfg(test)]` in `ui/theme.rs` and `ui/viewer.rs`

**Interfaces:**
- Renames `CARD_ROW_SLACK` to `CARD_ROW_GAP = 14.0`.
- Adds `pub(crate) fn viewer_nav_frame(t: f32, direction: f32) -> (f32, f32, f32, f32)`:
  - previous opacity, previous offset, current opacity, current offset.
  - At `t = 0`: `(1, 0, 0, direction * VIEWER_NAV_SLIDE)`
  - At `t = 1`: `(0, -direction * VIEWER_NAV_SLIDE, 1, 0)`
- Removes `viewer_nav_started_at` and `viewer_nav_progress`.
- Uses `self.preview.or_else(|| self.thumbnails.get(photo.id))` for the current image.
- Captures the visible image as `previous_preview` before `select_photo` in `step_viewer_photo`.

- [ ] **Step 1: Write the failing geometry tests**

```rust
assert_eq!(viewer_nav_frame(0.0, 1.0), (1.0, 0.0, 0.0, 28.0));
assert_eq!(viewer_nav_frame(1.0, 1.0), (0.0, -28.0, 1.0, 0.0));
assert_eq!(viewer_nav_frame(0.0, -1.0), (1.0, 0.0, 0.0, -28.0));
```

Update the existing `photo_row_height` assertions by +10:
```rust
close(photo_row_height(300.0, false, false), 214.0);
close(photo_row_height(300.0, true, false), 240.0);
close(photo_row_height(300.0, false, true), 237.0);
close(photo_row_height(300.0, true, true), 258.0);
```

- [ ] **Step 2: Run the tests and verify they fail**

Run: `cargo test -p vrc-photo-manager-ui viewer_nav_frame -j 1 -- --test-threads=1`
Run: `cargo test -p vrc-photo-manager-ui photo_row_height -j 1 -- --test-threads=1`
Expected: FAIL.

- [ ] **Step 3: Implement the gap and animation**

- Rename/raise the row gap and update the row-height tests.
- In `ui/viewer.rs`, remove `nav_driver` and the one-frame `nav_t` calculations. Render previous/current as absolutely positioned full-size images with `with_animation` keyed by `viewer_nav_epoch`; use `viewer_nav_frame`.
- Use `Duration::ZERO` when `nav_epoch == 0` or `cx.reduce_motion()`.
- In `app/viewer.rs`, capture the currently visible image before `select_photo`, then assign it to `previous_preview`; remove `viewer_nav_started_at` writes.
- Keep the current preview fallback to the thumbnail while decoding.

- [ ] **Step 4: Run the tests and verify they pass**

Run: `cargo test -p vrc-photo-manager-ui viewer_nav_frame -j 1 -- --test-threads=1`
Run: `cargo test -p vrc-photo-manager-ui photo_row_height -j 1 -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/ui/theme.rs crates/ui/src/ui/viewer.rs crates/ui/src/app.rs crates/ui/src/app/viewer.rs
git commit -m "fix(ui): increase card row gap and restore the viewer entrance animation"
```

---

### Task 6: Full verification and handoff

**Files:**
- No new files.

- [ ] **Step 1: Run formatting**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 2: Run clippy with parallelism disabled**

Run: `cargo clippy --workspace --all-targets -j 1 -- -D warnings`
Expected: no warnings/errors.

- [ ] **Step 3: Run tests with one thread**

Run: `cargo test --workspace -j 1 -- --test-threads=1`
Expected: all pass.

- [ ] **Step 4: Build the UI once for the user's visual check**

Run: `cargo build -p vrc-photo-manager-ui -j 1`
Expected: success.

- [ ] **Step 5: Hand off for visual verification**

Report the exact commands the user can run:
```bash
cargo run -p vrc-photo-manager-ui -j 1 -- --demo
```
Ask the user to check: bottom-right toasts, bell modal, sidebar tooltip, 14 px card gap, and viewer next/previous entrance.
