# Photo Note Editing and Tag Removal: Design

- Date: 2026-10-23
- Issue: [#10 feat: 写真詳細のメモ編集とタグ削除](https://github.com/TommyZ-7/vrc-photo-manager/issues/10)
- Status: Draft for review
- Target: `crates/core/src/db.rs`, `crates/ui/src/main.rs`, docs

## 1. Intent

The full-screen viewer's details panel shows a photo's note and tags, but the
note is read-only and tags can only be added. This change makes both editable
without leaving the viewer, and keeps the visible list and any active search
consistent with what was saved.

## 2. Goals

- Show a compact "編集" action next to the note, and switch the note section to
  a multi-line editor with "保存" and "キャンセル" actions.
- Save an empty or whitespace-only note as `NULL` (`Db::set_note(id, None)`), so
  the panel returns to "メモはまだありません" after reload.
- Add a delete control to every tag chip. Removing a tag writes the remaining
  tags with `Db::set_photo_tags`.
- Refresh the detail panel and selection inspector immediately after a save.
- Re-run the **executed** search filter asynchronously after a tag edit when the
  active search has a tag predicate, preserving the current scroll anchor.
- Keep keyboard shortcuts from firing while text is being entered. `Escape`
  inside the note editor cancels the edit instead of closing the viewer.

## 3. Non-goals

- Editing tags from the inspector card or from photo grid cards.
- Searching note text, rich text, markdown, autosave, undo/history, or a
  separate note-editing dialog.
- Changing the database schema or the semantics of `set_note` /
  `set_photo_tags`.
- Rebuilding the 100k-photo library index after a note or tag save. Neither
  value is part of `PhotoIndexEntry`; only the active search snapshot needs a
  refresh.

## 4. Approaches considered

### A. Re-run the executed search filter (chosen)

Capture the `PhotoTextFilter` used by the last search in `App`, re-run it
through the existing asynchronous search path after a tag edit, and restore
the top-left photo as the scroll anchor. This matches the database's `LIKE`
semantics exactly, avoids duplicating tag matching in the UI, and keeps the
search snapshot consistent even when the filter inputs have since been edited.

### B. Update only the detail and inspector

Smallest change, but a tag-filtered search left open behind the viewer would
keep showing a photo whose tag was just removed. It fails the issue's
"タグ検索の結果から外れる" acceptance criterion.

### C. Remove one row from the search snapshot

Fast and preserves scroll, but the UI would have to re-implement SQL `LIKE`
matching (including `%`/`_` wildcards and case behavior). The duplication is
more fragile than re-running the existing query.

## 5. Decisions

### Note editor state

- `App` gains `note_edit_input: Entity<TextareaState>` and
  `note_edit_id: Option<i64>`.
- Opening the editor sets `note_edit_id` to the displayed photo and copies the
  current note into the textarea.
- `load_selected_meta` clears the editor when the photo id changes. Closing the
  viewer or switching photos therefore discards an unsaved draft.
- Saving normalizes the value with `trim()`. An empty result is stored as
  `None`; otherwise the trimmed text is stored.
- Save failures use the existing `message` line in the details panel.
- Focus handles are added for edit/save/cancel. They join
  `quick_look_focus_order` in visual order: favorite, tag input, add tag,
  note controls, compress, trash. While editing, the textarea replaces the
  edit action in the order.
- `Escape` is handled by the existing `CloseQuickLook` action: if a note edit
  is active, cancel it; otherwise close the viewer. The explicit "閉じる"
  button still closes the viewer without saving.

### Tag removal

- Each tag chip gets a small `×` control. Clicking it removes that tag from
  `selected_tags`, writes the remainder with `set_photo_tags`, reloads the
  photo metadata, and refreshes the active search.
- The same save helper is used by tag addition, so both directions keep the
  search snapshot consistent.
- Removing the last tag leaves the existing "—" empty state.

### Search refresh

- `App` stores the last executed filter in `applied_search`, set by
  `run_search` and cleared by `clear_search`.
- A tag edit refreshes the search only when `search_active` is true and the
  applied filter contains a non-empty tag predicate. The refresh reuses
  `start_search` (the existing async path and generation guard) rather than
  reading the live filter inputs.
- The top-left photo id is captured before the refresh as
  `pending_search_anchor`. When the result arrives, the grid scrolls to that
  photo if it still exists, otherwise to the top. A normal search clears the
  anchor and starts at the top as before.

## 6. Failure handling

- Database write errors are shown in the details panel's `message` line; the
  editor stays open with the draft intact.
- A failed search refresh leaves the previous search snapshot mounted (the
  existing async path reports the error and does not replace the view).

## 7. Left to the builder

- Exact chip hit area, colors, and spacing within the existing design tokens.
- Wording of the save/cancel controls and whether to show a short "Esc で
  キャンセル" hint.
- Test names and the precise set of UI helper tests.
- Updating `docs/ui.md` and `docs/features.md` in the separate docs PR.
