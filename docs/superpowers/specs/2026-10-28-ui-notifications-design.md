# UI notifications and chrome cleanup — design

## Intent

Reduce the visual noise in the main window and make feedback consistent. The
header and the top of the content currently carry several overlapping status
surfaces: a ribbon status group, persistent failure bars, a message strip, an
import result panel, and a library error row. At the same time, a few layout
and motion defects make the sidebar and the viewer feel broken.

The change separates **transient feedback** (toasts) from **items that need the
user's attention** (a notification modal opened from the header bell), removes
the duplicated chrome, and fixes the four layout/motion defects.

## Goals

- Show transient success, info, warning, and error messages as bottom-right
  toasts that disappear after five seconds.
- Keep action-required items (import result, library load failure, compression
  failures, daemon errors) in one notification modal opened from the bell.
- Remove the duplicate notification surfaces from the top of the content and
  the header status/daemon/sync readouts.
- Restore the v0.7.0 sidebar toggle placement, move the navigation to the top,
  and remove the meaningless daemon dot row.
- Increase the library card vertical gap to 14 px.
- Restore the viewer's photo-switch entrance animation.
- Make the collapsed-sidebar tooltip render as an overlay in the correct
  position.
- Update `docs/ui.md` in a separate docs-only PR after the code lands.

## Non-goals

- No persistent notification history across restarts. The notification modal
  shows current action-required items only.
- No redesign of the dashboard, compression queue, or import pipeline.
- No new undo action for the trash. The toast reports the existing result.
- No change to the viewer's open/close morph or details panel behavior beyond
  the photo-switch animation.
- No change to the library paging, `uniform_list` virtualization, or year rail
  behavior.

## Decisions

| Topic | Decision |
| --- | --- |
| Transient toast implementation | `gpui-component` `NotificationList` (already mounted by the app's Root) |
| Toast position / lifetime | Bottom-right, 5 seconds, closable, stacked |
| Failure alerts | Toast only when the failure count increases; no persistent top bar |
| Failure baseline | The first successful count read seeds the baseline; existing failures do not toast on launch |
| Startup messages | Queued until the first render, then flushed as toasts |
| Actionable toast clicks | Only action-required toasts open the notification modal |
| Notification modal contents | Action-required items only |
| Notification history | None (session state is not persisted) |
| Bell badge | A dot while an action-required item exists |
| Import result | Summary + retry/resume in the modal; details collapsed by default |
| Library load error | Error text + reload in the modal |
| Header status group | Removed entirely, including selection count and bytes |
| Header daemon and sync | Removed |
| Sidebar | Floating v0.7.0-style toggle; menu starts at the top; daemon dot row removed |
| Library vertical card gap | 14 px (matches the horizontal gap) |
| Viewer switch animation | Both images overlaid and animated; thumbnail fallback while the preview decodes |
| Tooltip | Absolute overlay; animation no longer switches it to relative flow |

## Approaches considered

### Toasts

1. **`gpui-component` `NotificationList` (chosen).** The component already owns
   stacking, enter/exit animations, auto-hide, close-on-hover, and placement.
   The app only needs a `WindowHandle` stored on `App` so code without a
   `&mut Window` can push a toast. The visual language is the shared component
   theme, which the app already uses for dialogs.
2. **App-owned toast queue.** Full control over the exact panel styling, but it
   would add a second lifecycle (timers, closing state, stack ordering) next to
   the existing component notification system. Rejected as unnecessary.

### Notification modal

The modal is app-specific: it hosts the import report summary with retry,
resume, and collapsible details, plus failure counts with navigation actions.
A generic dialog component cannot own that content cleanly, so the modal is a
custom overlay rendered by `App`, like the viewer and import overlays. It uses
the existing panel colors, gets a focus handle, and participates in
`overlay_open()` so global shortcuts do not fire underneath it.

## Behavior

### Toasts

- `App` queues every toast and schedules a deferred flush from `render` with
  `cx.defer_in`. The flush calls `window.push_notification` after the current
  window update has finished, so event-handler toasts never re-enter GPUI's
  window update. Helper methods `toast_info`, `toast_success`, `toast_warning`,
  and `toast_error` enqueue the shared `ToastSpec`.
- Toasts appear bottom-right with a 340 px width and 16 px margins. The app
  theme is dark, so the component's popover colors match the panels.
- A toast disappears after five seconds. Only toasts that correspond to an
  action-required item (import report, library error, compression failure,
  daemon error) open the notification modal when clicked. Rejected operations
  such as `先に写真を選択してください` are plain error toasts and do not open a
  modal that might be empty.
- Existing `App.message` assignments are classified:
  - successful saves, moves to trash, retries, profile changes → success;
  - progress and state notes → info;
  - recoverable warnings → warning;
  - failures and rejected operations → error.
- Failure bars are not rendered. Instead:
  - compression failed count increase → one error toast with the count;
  - daemon error count increase → one error toast with the count;
  - import report arrives → one toast with imported/failed counts; partial
    moves and scan failures make it a warning rather than a success;
  - library load error is set → one error toast.
- Repeated counts do not re-toast on every refresh; only a transition to a
  higher count does. The first successful read of **both** compression and
  daemon counts seeds the baseline, so failures that already existed at launch
  are visible in the bell/modal but do not produce a startup toast. A failed
  count read leaves the previous baseline untouched.
- Messages produced during `App::new` are queued because the component Root
  plugin is not available until construction finishes. The queue is flushed
  with `cx.defer_in` on the first render, after the notification list exists.


### Notification modal

- The header bell becomes a button. A 4 px amber dot is shown while any of
  these is true:
  - `import_report` exists;
  - `library_error` exists;
  - `failed_job_count > 0`;
  - `daemon_errors` is not empty.
- Opening the modal focuses the modal container and installs a focus trap, so
  Tab/Shift+Tab stay inside. The close button remains focusable. It closes on
  the close button, a backdrop click, and `Esc`.
- The modal refuses to open while the import dialog or manual compression
  dialog is open; the existing overlay keeps focus instead of stacking.
- Sections, in order:
  1. **取り込み結果** (when a report exists): mode, destination, counts for
     imported/skipped/failed/partial/remaining/renamed/scan failures. Buttons:
     `失敗分を再試行` (when retryable), `残り N 件を再開` (when remaining),
     `詳細` toggle, `結果を閉じる`. The expanded area reuses the current
     failed-file, partial-move, renamed-file, scan-failure, and scan-error
     lists.
  2. **ライブラリ**: the error text and a `再読込` button.
  3. **圧縮**: failed count and a `ダッシュボードを開く` button. A
     `失敗を再試行` shortcut may be shown when failed jobs exist.
  4. **デーモン**: error count and a `ダッシュボードを開く` button.
- If none of the sections is present, the modal shows
  `対応が必要な通知はありません`.
- Retry/resume buttons close the notification modal before opening the import
  dialog so two overlays do not stack.
- Opening the notification modal from a toast while the viewer is open stacks
  the modal above the viewer; closing it returns to the viewer. The viewer is
  not closed implicitly. The notification modal and import dialog never stack
  because retry/resume closes the modal first.
- The sidebar status card's `結果を見る` action opens the notification modal
  instead of switching to the Photos screen.
- The viewer details panel no longer renders `App.message`. Feedback that
  happens while the viewer is open (tag save, note save, trash) appears in the
  toast layer above the viewer instead.

### Header and ribbon

- Remove the ribbon's `現在の状態` group, its message/selection label, and the
  `status_note` helper.
- Remove `active_ribbon_tab`, `RibbonTab`, `default_ribbon_tab`,
  `message_strip_visible`, `ribbon_status_visible`, `ribbon_selection_label`,
  `titlebar_sync_visible`, `titlebar_sync_label`, and the shell's
  `message_strip`, `global_alerts`, and `alert_summaries`.
- Remove the daemon status pill and the calendar/sync label from the titlebar.
- Keep the app glyph, app name, screen name, ribbon collapse button, bell, and
  the standard window controls.
- `ChromeLayout` keeps only the content dimensions, ribbon height/metrics, and
  the compact-ribbon flag.

### Sidebar

- Remove the 34 px in-flow toggle row and the 12 px spacer below it. The
  navigation list starts at the sidebar's 16 px top padding and reserves an
  18 px right gutter for the floating toggle; the active pill uses the same
  gutter so it does not extend under the toggle.
- Recreate the v0.7.0 toggle: a 24 px round ghost button on the sidebar's right
  edge, positioned at `WORKSPACE_PAD + sidebar_width - 12` horizontally and
  `WORKSPACE_PAD + 18` vertically, with the same 440 ms slide animation as the
  sidebar width.
- Keep the `Ctrl/Cmd+B` binding and the tooltip.
- Remove the bottom daemon dot row. Daemon state remains visible in the
  sidebar's scan status tab and on the Dashboard.
- Keep the library count/storage card and the version label.

### Collapsed-sidebar tooltip

- Keep the custom tooltip element for immediate labels and the existing style.
- Its animation must keep the element absolutely positioned: animate `left`
  from `target - 4` to `target` instead of returning `el.relative()`.
- The target is `WORKSPACE_PAD + sidebar_width(true) + 20` horizontally (after
  the floating toggle) and `WORKSPACE_PAD + 16 + nav_item_top(index) + 8.5`
  vertically. This follows the menu moving to the top and no longer depends on
  the removed toggle row.
- The tooltip is painted after the main column so it overlays the content.

### Library grid

- Rename `CARD_ROW_SLACK` to `CARD_ROW_GAP` and set it to 14 px.
- `photo_row_height` adds the gap after the image and metadata, so the visible
  space between a card's bottom and the next card's image top is 14 px.
- The year rail, anchor calculations, and scroll math continue to use
  `row_height`; no new geometry path is introduced.

### Viewer photo switch

- Remove the zero-size `nav_driver` that only forced redraws.
- Place the previous and current images in the same frame as absolutely
  positioned, full-size children so both are visible during the transition.
- Animate both with `with_animation` keyed by `viewer_nav_epoch`:
  - previous: opacity `1 - t`, offset `-direction * VIEWER_NAV_SLIDE * t`;
  - current: opacity `t`, offset `direction * VIEWER_NAV_SLIDE * (1 - t)`.
- Use `self.preview` for the current image and fall back to
  `self.thumbnails.get(photo.id)` while the preview decodes. This restores the
  documented thumbnail placeholder and guarantees an entrance image.
- A current image that arrives after the animation starts keeps the same
  animation key and replaces the thumbnail in place.
- Remove `viewer_nav_started_at` and `viewer_nav_progress` if no longer used.
- Reduced-motion behavior stays: no slide/fade when `cx.reduce_motion()` is
  true.

## Files

- `crates/ui/src/app.rs`: notification modal state, window handle, toast queue,
  overlay guard, remove `message`/`active_ribbon_tab`.
- `crates/ui/src/app/*.rs`: replace `message` writes with toast helpers; detect
  failure-count transitions; add import-report toast.
- `crates/ui/src/update_view.rs`: replace update-flow `message` writes with
  toast helpers. The update card's own phase/notice display stays.
- `crates/ui/src/ui/chrome.rs`: remove status group, daemon pill, sync label;
  bell opens the modal.
- `crates/ui/src/ui/shell.rs`: remove alerts/message strip; sidebar layout;
  tooltip; open modal from the import status card.
- `crates/ui/src/ui/notifications.rs` (new): toast helpers, notification modal,
  item builders, focus/open/close.
- `crates/ui/src/ui/import.rs`: split the result panel into summary + details
  for the modal.
- `crates/ui/src/ui/library.rs`: remove the top error row and inline message;
  use the new row gap.
- `crates/ui/src/ui/search.rs`: remove the message readout.
- `crates/ui/src/ui/viewer.rs`: overlay images and per-frame animation; remove
  the details-panel message block.
- `crates/ui/src/ui/theme.rs`: `CARD_ROW_GAP`.
- `crates/ui/src/ui/actions.rs`, `main.rs`: `CloseNotifications` action and
  `Esc` binding for the modal.
- `crates/ui/src/main.rs`: notification placement/margins/width.

## Testing

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Unit tests:
  - sidebar tooltip target follows the top-aligned menu;
  - `photo_row_height` includes the 14 px gap;
  - overlay guard includes the notification modal;
  - notification item selection (import report, library error, compression,
    daemon) and empty state;
  - failure-baseline seeding suppresses launch toasts and later increases toast;
  - pending startup toasts flush once;
  - viewer image plan/overlay does not depend on a flow sibling.
- Visual verification is performed by the user. The app should be checked with
  `--demo` for the toast, modal, sidebar tooltip, card gap, and viewer switch.

## Open questions

None. The remaining choices (exact component toast styling, icon choices, and
modal spacing) are implementation details that do not change the behavior
above.
