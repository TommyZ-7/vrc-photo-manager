# Photo Note Editing and Tag Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add note editing (save/cancel, empty → `NULL`) and per-tag removal to the full-screen viewer's details panel, and keep the active search snapshot consistent after tag edits.

**Architecture:** Extend `App` with a `TextareaState` draft and an edit id, plus the last executed `PhotoTextFilter` and a pending scroll anchor. Note saves call the existing `Db::set_note`; tag edits call `Db::set_photo_tags` and, when the active search has a tag predicate, re-run the stored filter through the existing async search path and restore the top-left photo. No schema or library-index changes.

**Tech Stack:** Rust 2024, `gpui-kit = "=0.8.0"` / `gpui-pre 0.3.9`, `gpui-component` `Textarea`, SQLite via `rusqlite 0.40`.

**Spec:** `docs/superpowers/specs/2026-10-23-note-tag-edit-design.md`

## Global Constraints

- Do not add Cargo dependencies or bump `gpui-kit`.
- Keep `crates/ui/src/main.rs` as the single UI file.
- Do not change the database schema or the behavior of `Db::set_note` / `Db::set_photo_tags`.
- Do not rebuild the 100k-photo library index after a note or tag save; only the search snapshot may refresh.
- Any search refresh must go through the existing asynchronous `start_search` path and its generation guard. Never query SQLite from `Render`/`prepaint`/`paint`.
- Keep documentation in a separate docs-only PR per `AGENTS.md` §4.
- Work in a git worktree on `feat/issue-10-note-tag-edit`; never push to `main`.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Do not create tags or run the Release workflow.

## Review Focus

These are the failure modes most likely to survive a happy-path implementation. Each is pinned by a test or an explicit verification step.

1. **Empty note becomes a placeholder again** after save and reload, including whitespace-only input. Pinned by the DB test in Task 1 and the `normalize_note` test in Task 2.
2. **`Escape` while editing cancels the draft and leaves the viewer open.** There is no GUI test harness; Task 4 verifies it in the demo app.
3. **Typing in the note field does not fire viewer shortcuts** (`Space`, `Left`, `Right`, `Tab`, `Escape`). Task 4 verifies it in the demo app.
4. **Removing the only matching tag drops the photo from an active tag search** while the rest of the result set and the scroll anchor survive. Pinned by the DB test in Task 1 and the anchor/re-query path in Task 3; Task 4 verifies the visible behavior.
5. **A stale search response cannot overwrite a newer edit.** The existing `search_request` generation guard must remain in the refactored `start_search`; Task 3 keeps the guard and Task 4 checks a quick tag removal followed by a new search.

---

### Task 1: Pin note and tag persistence at the database layer

**Files:**
- Modify: `crates/core/src/db.rs` (tests near `photo_index_text_applies_favorite_filter`, around line 1443)

**Interfaces:**
- Consumes: `Db::set_note(i64, Option<&str>)`, `Db::set_photo_tags(i64, &[String])`, `Db::photo_by_id(i64)`, `Db::photo_tags(i64)`, `Db::photo_index_text(&PhotoTextFilter)`, `photo_at(...)`, `PhotoTextFilter`.
- Produces: regression coverage only; no production signature changes.

- [ ] **Step 1: Add the two regression tests**

Append after `photo_index_text_applies_favorite_filter`:

```rust
#[test]
fn set_note_round_trips_and_clears_with_none() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let id = db
        .upsert_photo(&photo_at(
            root,
            "/photos/note.png",
            1_700_000_000,
            "2025-04-30 09:00:00",
        ))
        .unwrap();

    assert_eq!(db.photo_by_id(id).unwrap().unwrap().note, None);
    db.set_note(id, Some("first note")).unwrap();
    assert_eq!(
        db.photo_by_id(id).unwrap().unwrap().note.as_deref(),
        Some("first note")
    );
    db.set_note(id, None).unwrap();
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().note, None);
}

#[test]
fn removing_a_tag_drops_the_photo_from_tag_search() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let id = db
        .upsert_photo(&photo_at(
            root,
            "/photos/tagged.png",
            1_700_000_000,
            "2025-04-30 09:00:00",
        ))
        .unwrap();
    db.set_photo_tags(id, &["keep".to_string(), "remove".to_string()])
        .unwrap();

    let filter = PhotoTextFilter {
        tag: Some("remove".to_string()),
        ..Default::default()
    };
    assert_eq!(db.photo_index_text(&filter).unwrap().len(), 1);

    db.set_photo_tags(id, &["keep".to_string()]).unwrap();
    assert!(db.photo_index_text(&filter).unwrap().is_empty());
    assert_eq!(db.photo_tags(id).unwrap(), vec!["keep".to_string()]);
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p vrc-photo-core`
Expected: PASS. These pin behavior that already exists; if either new test fails, fix the DB contract before continuing.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "test: cover note clearing and tag removal persistence (#10)"
```

---

### Task 2: Add the note editor to the viewer details panel

**Files:**
- Modify: `crates/ui/src/main.rs` (imports around line 12; `App` around 1035–1111; `App::new` around 1116–1228; `load_selected_meta` around 1499; `open_quick_look` / `close_quick_look` around 1598–1643; `quick_look_focus_order` around 1726; `detail_body` around 3468; root `CloseQuickLook` handler around 5157; test module around 5870)

**Interfaces:**
- Consumes: `Textarea`, `TextareaState`, `focusable_text_button`, `quick_look_focus_order`, existing focus handles.
- Produces:
  - `fn normalize_note(value: &str) -> Option<&str>`
  - `App.note_edit_input: Entity<TextareaState>`
  - `App.note_edit_id: Option<i64>`
  - `App.quick_look_note_edit_focus: FocusHandle`
  - `App.quick_look_note_save_focus: FocusHandle`
  - `App.quick_look_note_cancel_focus: FocusHandle`
  - `App::begin_note_edit(&mut self, id: i64, note: Option<String>, window: &mut Window, cx: &mut Context<Self>)`
  - `App::save_note(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>)`
  - `App::cancel_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `App::handle_viewer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>)`

- [ ] **Step 1: Write the failing normalization test**

Add `normalize_note` to the explicit import list in `mod tests`, then add:

```rust
#[test]
fn notes_normalize_empty_to_none_and_trim_text() {
    assert_eq!(normalize_note(""), None);
    assert_eq!(normalize_note("  \n\t "), None);
    assert_eq!(normalize_note(" hello \n"), Some("hello"));
    assert_eq!(normalize_note("line1\nline2"), Some("line1\nline2"));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui notes_normalize_empty_to_none_and_trim_text`
Expected: FAIL with an unresolved import or missing function.

- [ ] **Step 3: Implement `normalize_note`**

Place it near the other pure helpers (`format_bytes` / `next_focus_index`):

```rust
/// Trims the outer whitespace of a note and treats an empty result as "no note".
fn normalize_note(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vrc-photo-ui notes_normalize_empty_to_none_and_trim_text`
Expected: PASS.

- [ ] **Step 5: Add the textarea import, state, and focus handles**

Change the input import to:

```rust
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
```

In `App`, after `tag_edit_input`, add:

```rust
note_edit_input: Entity<TextareaState>,
note_edit_id: Option<i64>,
```

After `quick_look_add_tag_focus`, add:

```rust
quick_look_note_edit_focus: FocusHandle,
quick_look_note_save_focus: FocusHandle,
quick_look_note_cancel_focus: FocusHandle,
```

In `App::new`, create the textarea:

```rust
let note_edit_input =
    cx.new(|cx| TextareaState::new(window, cx).placeholder("メモを入力").auto_grow(2, 6));
```

Create the focus handles next to the other `quick_look_*_focus` handles, then add all four fields to the `Self { ... }` literal (`note_edit_input`, `note_edit_id: None`, and the three focus handles).

- [ ] **Step 6: Add the editor methods**

Add after `load_selected_meta`:

```rust
fn begin_note_edit(
    &mut self,
    id: i64,
    note: Option<String>,
    window: &mut Window,
    cx: &mut Context<Self>,
) {
    self.note_edit_id = Some(id);
    let text = note.unwrap_or_default();
    self.note_edit_input
        .update(cx, |input, cx| input.set_value(text, window, cx));
    let focus = self.note_edit_input.read(cx).focus_handle(cx);
    window.focus(&focus, cx);
    cx.notify();
}

fn save_note(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
    let value = self.note_edit_input.read(cx).value();
    let note = normalize_note(&value);
    if let Err(error) = self.db.set_note(id, note) {
        self.message = Some(format!("メモを保存できません: {error}"));
        cx.notify();
        return;
    }
    self.note_edit_id = None;
    self.load_selected_meta(id);
    window.focus(&self.quick_look_note_edit_focus, cx);
    cx.notify();
}

fn cancel_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.note_edit_id.take().is_none() {
        return;
    }
    window.focus(&self.quick_look_note_edit_focus, cx);
    cx.notify();
}

fn handle_viewer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.note_edit_id.is_some() {
        self.cancel_note_edit(window, cx);
    } else {
        self.close_quick_look(window, cx);
    }
}
```

- [ ] **Step 7: Reset the editor when the photo or panel changes**

- In `load_selected_meta`, after setting the fields, add:

```rust
if self.note_edit_id.is_some_and(|edit_id| edit_id != id) {
    self.note_edit_id = None;
}
```

- In `open_quick_look`, set `self.note_edit_id = None;` next to `self.delete_armed = None;`.
- In `close_quick_look`, set `self.note_edit_id = None;` next to `self.delete_armed = None;`.
- In `guard_viewer_selection`, set `self.note_edit_id = None;` next to `self.delete_armed = None;`.
- In `toggle_quick_look_details`, after toggling, add:

```rust
if !self.quick_look_details_open {
    self.note_edit_id = None;
}
```

- [ ] **Step 8: Add the note controls to the focus order**

Inside the `if self.quick_look_details_open` block of `quick_look_focus_order`, between the add-tag and compress entries, add:

```rust
if self.note_edit_id.is_some() {
    order.push(self.note_edit_input.read(cx).focus_handle(cx));
    order.push(self.quick_look_note_save_focus.clone());
    order.push(self.quick_look_note_cancel_focus.clone());
} else {
    order.push(self.quick_look_note_edit_focus.clone());
}
```

- [ ] **Step 9: Replace the read-only note block in `detail_body`**

Before the final `v_flex()`, build the edit/save/cancel controls with `focusable_text_button`:

- `viewer-note-edit`, label `編集`, focus `quick_look_note_edit_focus`, handler calls `begin_note_edit(id, note.clone(), window, cx)`.
- `viewer-note-save`, label `保存`, focus `quick_look_note_save_focus`, handler calls `save_note(id, window, cx)`.
- `viewer-note-cancel`, label `キャンセル`, focus `quick_look_note_cancel_focus`, handler calls `cancel_note_edit(window, cx)`.

Replace the existing note section with:

```rust
.child(
    v_flex()
        .gap(px(6.))
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(section("メモ"))
                .when(self.note_edit_id != Some(id), |this| this.child(note_edit_button)),
        )
        .when(self.note_edit_id == Some(id), |this| {
            this.child(Textarea::new(&self.note_edit_input).w_full())
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(note_save_button)
                        .child(note_cancel_button)
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("Esc でキャンセル"),
                        ),
                )
        })
        .when(self.note_edit_id != Some(id), |this| {
            this.child(
                div()
                    .p(px(8.))
                    .rounded(px(8.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child(
                        photo
                            .note
                            .clone()
                            .unwrap_or_else(|| "メモはまだありません".to_string()),
                    ),
            )
        }),
)
```

Clone `note`, `note_edit_button`, `note_save_button`, and `note_cancel_button` before the builder chain as needed so the closures outlive the render.

- [ ] **Step 10: Route the viewer's `Escape` action through the editor check**

In `impl Render for App`, change the `CloseQuickLook` handler from `this.close_quick_look(window, cx)` to `this.handle_viewer_escape(window, cx)`.

- [ ] **Step 11: Run the focused tests and clippy**

Run: `cargo test -p vrc-photo-ui` then `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS with no warnings.

- [ ] **Step 12: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: edit and save photo notes in the viewer (#10)"
```

---

### Task 3: Add tag removal and refresh the executed search

**Files:**
- Modify: `crates/ui/src/main.rs` (`App` fields around 1035–1111; `App::new` around 1116–1228; `run_search` around 1834; `apply_search_result` around 1901; `clear_search` around 1930; `detail_body` tag section around 3647–3662; add helpers near `run_search` / `detail_body`)

**Interfaces:**
- Consumes: `PhotoTextFilter`, `PhotoBrowser.index`, `PhotoBrowser.scroll`, `PhotoBrowser.columns`, `anchor_photo_id`, `list_scroll_top`, `uniform_list` scroll strategy already imported.
- Produces:
  - `App.applied_search: Option<PhotoTextFilter>`
  - `App.pending_search_anchor: Option<i64>`
  - `App::start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>)`
  - `App::refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>)`
  - `App::commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>)`
  - `App::remove_tag(&mut self, photo_id: i64, tag: &str, cx: &mut Context<Self>)`
  - `App::tag_chip(&self, photo_id: i64, tag: String, cx: &mut Context<Self>) -> AnyElement`

- [ ] **Step 1: Add the search state fields**

In `App`, next to `search_request`, add:

```rust
applied_search: Option<PhotoTextFilter>,
pending_search_anchor: Option<i64>,
```

In `App::new`, initialize both to `None`.

- [ ] **Step 2: Split `run_search` from the async start path**

Keep the current `if self.searching { return; }` and filter construction in `run_search`. After constructing `filter`, add:

```rust
self.applied_search = Some(filter.clone());
self.pending_search_anchor = None;
self.start_search(filter, cx);
```

Move the existing generation/async/fallback body (from `self.search_request = ...` onward) into:

```rust
fn start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>) {
    self.search_request = self.search_request.wrapping_add(1);
    let generation = self.search_request;
    // Existing body: in-memory fallback, path lookup, searching flag,
    // background query, generation check, apply_search_result.
}
```

Do not add a `searching` guard inside `start_search`; callers already check it.

- [ ] **Step 3: Restore the pending anchor when applying a result**

In `apply_search_result`, after `replace_index`, replace the scroll-to-top call with:

```rust
let columns = self.search_view.columns.max(1);
let row = self
    .pending_search_anchor
    .take()
    .and_then(|id| self.search_view.index.row_of_id(id));
match row {
    Some(row) => self
        .search_view
        .scroll
        .scroll_to_item_strict(row / columns, ScrollStrategy::Top),
    None => self
        .search_view
        .scroll
        .scroll_to_item_strict(0, ScrollStrategy::Top),
}
```

In the error arm, add `self.pending_search_anchor = None;`.

In `clear_search`, add:

```rust
self.applied_search = None;
self.pending_search_anchor = None;
```

- [ ] **Step 4: Add the tag edit and search refresh helpers**

Add near `run_search`:

```rust
fn refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>) {
    if !self.search_active || self.searching {
        return;
    }
    let Some(filter) = self.applied_search.clone() else {
        return;
    };
    let has_tag = filter
        .tag
        .as_deref()
        .map(str::trim)
        .is_some_and(|tag| !tag.is_empty());
    if !has_tag {
        return;
    }
    self.pending_search_anchor = anchor_photo_id(
        &self.search_view.index,
        list_scroll_top(&self.search_view.scroll),
        self.search_view.columns,
    );
    self.start_search(filter, cx);
}

fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
    if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
        self.message = Some(format!("タグを保存できません: {error}"));
        cx.notify();
        return;
    }
    self.load_selected_meta(photo_id);
    self.refresh_search_after_tag_edit(cx);
    cx.notify();
}

fn remove_tag(&mut self, photo_id: i64, tag: &str, cx: &mut Context<Self>) {
    let tags = self
        .selected_tags
        .iter()
        .filter(|existing| existing.as_str() != tag)
        .cloned()
        .collect();
    self.commit_tags(photo_id, tags, cx);
}
```

- [ ] **Step 5: Route tag addition through the same helper**

In the add-tag button handler, replace the direct `set_photo_tags` / `load_selected_meta` calls with:

```rust
let mut tags = this.selected_tags.clone();
if tags.contains(&tag) {
    return;
}
tags.push(tag);
this.commit_tags(id, tags, cx);
```

- [ ] **Step 6: Render removable tag chips**

Add a helper method:

```rust
fn tag_chip(
    &self,
    photo_id: i64,
    tag: String,
    cx: &mut Context<Self>,
) -> AnyElement {
    let remove = tag.clone();
    h_flex()
        .id(SharedString::from(format!("viewer-tag-{tag}")))
        .items_center()
        .gap(px(3.))
        .pl(px(7.))
        .pr(px(3.))
        .py(px(2.))
        .rounded_full()
        .bg(rgba(0xffffff0f))
        .border_1()
        .border_color(rgba(0xffffff12))
        .text_size(px(10.))
        .text_color(rgb(MUTED))
        .child(format!("#{tag}"))
        .child(
            div()
                .id(SharedString::from(format!("viewer-tag-remove-{tag}")))
                .cursor_pointer()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_color(rgb(MUTED_2))
                .hover(|this| this.bg(rgba(0xfb71852e)).text_color(rgb(RED)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.remove_tag(photo_id, &remove, cx);
                }))
                .child("×"),
        )
        .into_any_element()
}
```

In `detail_body`, replace the tag `.children(tags.iter().map(...))` with:

```rust
.children(
    tags.iter()
        .map(|tag| self.tag_chip(id, tag.clone(), cx))
        .collect::<Vec<_>>(),
)
```

Keep the existing `when(tags.is_empty(), |this| this.child(pill("—".to_string())))` branch.

- [ ] **Step 7: Run the focused tests and clippy**

Run: `cargo test -p vrc-photo-ui` then `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS with no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: remove photo tags and refresh active search (#10)"
```

---

### Task 4: Verify the whole change

**Files:** none (verification only).

- [ ] **Step 1: Run the full local gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all commands exit 0; tests report 0 failures.

- [ ] **Step 2: Verify note behavior in the demo app**

Run: `cargo run -p vrc-photo-ui -- --demo`

In the viewer details panel:
- Open a note, type text, click 保存; close and reopen the viewer; the text remains.
- Edit the note to empty, save; the panel shows "メモはまだありません".
- Start editing, press `Esc`; the viewer stays open and the old note remains.
- Start editing, click キャンセル; the old note remains.
- While the note field has focus, press `Space`, `Left`, `Right`, and `Tab`; the viewer must not navigate or close.

- [ ] **Step 3: Verify tag removal and search refresh in the demo app**

- Add two tags to a photo, then remove one with `×`; reopen the viewer and confirm only the remaining tag is present.
- Run a tag search that matches both tags, remove one matching tag in the viewer, close the viewer; the photo must drop out of the result set while the scroll position stays near the same photo row.
- Remove the last tag; the tag section shows "—".
- Remove a tag while a search is running, then immediately start a new search; the newest result must win (no stale overwrite).

- [ ] **Step 4: Update user docs in the separate docs-only PR**

In `docs/ui.md`, `docs/features.md`, and `docs/testing.md`, replace "メモ表示（編集は未実装）", "タグ追加", and the "未実装" entries with the new behavior and checklist items. Keep the design and plan in that docs PR; do not mix them into the code PR.
