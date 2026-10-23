# Compression Profile Management: Design

- Date: 2026-10-23
- Issue: [#11 feat: 圧縮プロファイル管理（並び替え・複製・削除確認）](https://github.com/TommyZ-7/vrc-photo-manager/issues/11)
- Status: Approved in session
- Target: `crates/core/src/db.rs`, `crates/daemon/src/lib.rs`, `crates/ui/src/main.rs`, docs

## 1. Intent

The compression screen can create and edit profiles, but the list cannot be
reordered, profiles cannot be duplicated, and `Db::delete_profile` has no
caller. The `sort_order` column is written as `0` for every row, so the list
and automatic compression silently fall back to name order. This change makes
profile management complete: a user can move profiles, duplicate one, and
delete one with a confirmation dialog, and the saved order becomes the order
used by automatic compression.

## 2. Goals

- Persist a user-visible profile order in `compression_profiles.sort_order`
  and keep it after restart.
- Make automatic compression enqueue jobs in that same order.
- Add an up/down control to every profile card; the top/bottom edge disables
  the corresponding direction.
- Add "複製" to the selected profile. The copy gets a unique name, is inserted
  directly after its source, and can be edited independently.
- Add "削除" with a modal confirmation dialog. Cancelling deletes nothing.
- Block deletion while a `pending` or `processing` job references the
  profile, and explain why.
- Keep at least one profile: deleting the last one is blocked with a message.
- Close the delete/enqueue race so automatic compression cannot create a job
  for a profile that has already been deleted.
- Keep the selected profile stable across moves, duplication, and deletion
  where possible.
- Keep completed/failed/cancelled jobs and compression derivatives as history
  when a profile is deleted.

## 3. Non-goals

- Drag-and-drop reordering.
- Renaming a profile (the name remains fixed for now).
- Deleting jobs or derivatives together with a profile.
- Changing the "seed defaults when the table is empty" startup rule or adding
  a seeded marker. The last-profile guard means the table is never emptied by
  this UI.
- Changing the database schema, `user_version`, or migrations.
- Changing which profiles are eligible for automatic compression
  (`enabled && auto_enabled`).
- A general dialog framework for the rest of the app.

## 4. Current behavior and constraints

- `save_profile` inserts `sort_order = 0` and the `ON CONFLICT` update leaves
  the column untouched. Existing databases therefore have every row at `0`.
- `list_profiles` orders by `sort_order, name COLLATE NOCASE`; with all-zero
  rows the visible order is name order.
- `enqueue_auto_jobs` iterates `list_profiles()` in that order, so fixing the
  stored order fixes automatic compression order without a new query.
- `compression_jobs.profile_id` has no foreign key. `process_job` resolves the
  profile by id when the job starts; deleting a profile with a live job would
  make that job fail with "compression profile no longer exists".
- `derivatives.profile_id` is also a plain string kept for history.
- The app already wraps its window in `gpui_base::Root` through
  `gpui_kit::open_window`, and `gpui_kit::init` registers the component plugin.
  `WindowExt::open_alert_dialog` is therefore available without restructuring
  the window.
- There is no GUI test harness in the UI crate. Pure ordering helpers are unit
  tested; visual behavior is verified in the demo app.

## 5. Approaches considered

### 5.1 Order persistence

**A. Store the complete order (chosen).** `Db::reorder_profiles(ids)` validates
that the id list covers every row and writes contiguous `sort_order` values in
one transaction. The UI swaps two adjacent ids and sends the whole list. This
normalizes legacy all-zero rows the first time a profile is moved, supports
future drag-and-drop without another API, and makes corruption detectable.

**B. Swap two rows.** A `swap_profile_order(a, b)` method is smaller, but with
legacy all-zero rows the swap is a no-op until some other code normalizes
them. It also cannot repair duplicate `sort_order` values.

**C. Sort only in the UI.** Fast, but the order would not survive restart and
automatic compression would keep using name order. It fails the acceptance
criteria.

### 5.2 Duplication

**A. Transactional `Db::duplicate_profile(id)` (chosen).** Reads the ordered
rows, normalizes the order, computes a unique name, shifts later rows, inserts
the copy after its source, and commits. One call is atomic and testable
against an in-memory database.

**B. Clone in the UI and call `save_profile`.** Smaller, but the copy would go
to the end, name uniqueness would live in the UI with no database test, and a
failure between insert and reorder would leave a half-applied order.

### 5.3 Delete confirmation

**A. `gpui-component` `AlertDialog` (chosen).** The issue asks for a
confirmation dialog. The component library is already a dependency and its
root plugin is active. The dialog gets a title, the profile name, a danger
"削除" button, and a "キャンセル" button.

**B. Two-step button.** This matches the existing photo trash control and is
smaller, but it is not a dialog and is easier to trigger accidentally.

### 5.4 Active jobs

**A. Block deletion (chosen).** `active_job_count_for_profile` drives the
pre-check; `delete_profile` repeats the check inside its transaction so a job
enqueued between the dialog opening and confirmation still cannot be orphaned.
The user sees an explanatory message and the profile remains.

**B. Cancel pending jobs and delete.** Avoids leaving pending work behind, but a
job that already moved to `processing` still fails.

**C. Delete and let jobs fail.** Simplest, but turns a user action into
avoidable failed jobs.

### 5.5 Delete/enqueue race

The delete transaction protects jobs that already exist, but a daemon could
read the profile list, the UI could delete the profile, and the daemon could
then insert a job for the missing id. `enqueue_auto_jobs` therefore uses
`Db::enqueue_job_for_existing_profile`, which inserts only when the profile row
still exists:

- If the job insert commits first, the delete transaction sees it and blocks.
- If the delete commits first, the guarded insert inserts nothing.

The normal `enqueue_job` stays unchanged because the queue round-trip test and
manual enqueue paths intentionally allow a profile id that is not in the table
at insert time.

## 6. Design

### 6.1 Order storage

- `save_profile` inserts at `MAX(sort_order) + 1` (or `0` for the first row).
  The `ON CONFLICT` branch continues to update only name/enabled/auto_enabled/
  JSON, so an edit never changes a profile's position.
- `ensure_default_profiles` inserts the presets with contiguous 0..3 order, so
  a fresh database follows the preset order instead of name order.
- `reorder_profiles(&[String])`:
  - opens one transaction;
  - requires the id count to equal the stored row count and rejects duplicate
    ids, otherwise returns `CoreError::Invalid`;
  - updates each id to its list index and returns `CoreError::NotFound` if an
    id matches no row;
  - commits only after every id succeeds, so a bad list leaves the old order
    intact.
- `delete_profile` leaves gaps in `sort_order`. That is harmless because
  `list_profiles` only sorts by the value; the next successful move or
  duplicate normalizes the range.

### 6.2 Duplication

`Db::duplicate_profile(id)` runs in one transaction:

1. Read `(id, json)` ordered by `sort_order, name`.
2. Find the source; return `CoreError::NotFound` if it is gone.
3. Normalize the existing rows to `0..n-1`.
4. Build the copy name: `"<source> のコピー"`, then `"<source> のコピー 2"`,
   `"<source> のコピー 3"`, ... until it is not present in any existing name.
5. Shift every row after the source by one.
6. Clone the source, assign a new UUID, set `auto_enabled = false`, and insert
   the copy at `source_index + 1`.
7. Commit and return the copy.

`auto_enabled = false` is deliberate: an identical auto-enabled copy with the
same output suffix would enqueue two compression jobs for every new photo.
The user can enable it after changing the format or suffix.

### 6.3 Deletion

- `active_job_count_for_profile(id)` counts `pending` and `processing` jobs.
- `delete_profile(id)` runs one transaction:
  - returns `CoreError::NotFound` if the id does not exist;
  - repeats the active-job count; if it is non-zero, returns
    `CoreError::Invalid` with the count and deletes nothing;
  - refuses to delete the last remaining profile with
    `CoreError::Invalid`, because `ensure_default_profiles` would otherwise
    recreate the presets with new ids on the next launch;
  - deletes the row and commits.
- Completed, failed, and cancelled jobs and all derivatives remain. They keep
  the profile id as history, which is safe because nothing joins it back to
  `compression_profiles`.
- The UI pre-checks the count and the one-profile minimum. On zero active jobs
  and at least two profiles it opens the `AlertDialog`; otherwise it writes a
  message such as
  "「WebP 高品質」は未完了のジョブが2件あるため削除できません" or
  "最後のプロファイルは削除できません" and does not open the dialog. The
  transactional re-check covers jobs already inserted; the guarded auto-enqueue
  in 5.5 closes the insert race. A failure at confirm time is mapped back to
  the same Japanese message.

### 6.4 Automatic compression order

- `enqueue_auto_jobs` iterates `list_profiles()` in stored order and uses the
  guarded insert from 5.5.
- The guarantee is **per newly indexed photo**: its auto profiles are enqueued
  in saved order, and the single default worker consumes them in that order.
  A batch of photos is enqueued photo-major, so the global queue is
  photo1/profile1, photo1/profile2, photo2/profile1, ... .
- Moving a profile affects future enqueues only; it does not reorder jobs that
  are already pending.
- With `compression_workers > 1`, dequeue order is still deterministic, but
  completion order is not. This change does not serialize workers.
- `next_pending_job` changes to `ORDER BY created_at, rowid`. Multiple profiles
  are enqueued in the same loop and can share a timestamp; rowid makes the
  insertion order deterministic so the worker consumes the first profile
  first.
- A daemon test enqueues for a reordered profile list and drains the pending
  queue to pin the per-photo sequence.

### 6.5 UI

- Every profile card gets compact `↑` / `↓` controls in its header. The first
  card's `↑` and the last card's `↓` are muted and have no handler. Clicking an
  arrow stops propagation so it does not also change the card selection; the
  card selection follows the moved profile.
- The editor header gets "複製" and a danger "削除" action before the auto
  switch.
- "複製" calls `Db::duplicate_profile`, reloads the list, selects the copy, and
  reports "「...」を複製しました（自動適用はOFF）".
- "削除" opens the modal only when the active-job count is zero and more than
  one profile remains. Confirming calls `Db::delete_profile`, reloads the
  list, selects the profile now at the removed position (or the new last row),
  and reports success. Cancel is a no-op.
- All failures use the existing header `message` line; the profile list and
  selection stay unchanged. A confirm-time active-job failure is shown with
  the same Japanese count message, not the raw `CoreError` text.
- A pure `moved_profile_order(ids, index, delta) -> Option<Vec<String>>` helper
  mirrors the DB call and is unit tested at both edges and in the middle.

## 7. Failure handling

- A failed reorder, duplicate, or delete leaves the in-memory list untouched
  and shows the database error in the header message.
- A failed profile reload after a successful write falls back to an empty list
  (`unwrap_or_default`), matching the existing code; the next `reload_aux`
  repopulates it.
- Closing the app while the confirmation dialog is open simply discards the
  pending action.

## 8. Risks

- Legacy databases have all rows at `0`. Moving once or duplicating once
  normalizes them; until then the visible order remains name order. This is
  accepted because it matches today's behavior.
- `delete_profile` now refuses active jobs and the last remaining profile.
  There is no other caller.
- The guarded auto-enqueue closes the window where a job could be inserted for
  a just-deleted profile. The normal manual `enqueue_job` remains permissive by
  design; the UI deletes and enqueues on the same thread.
- The AlertDialog is the first modal in the app. If the component layer's root
  plugin were missing, the dialog would not appear; the demo verification step
  checks it explicitly.

## 9. Verification

- Core tests: insert order, update preserves order, reorder normalizes and
  rejects bad or duplicate id lists, duplicate naming/position/independence/
  legacy zeros, delete blocking (active jobs and last profile) and history.
- Core test: guarded auto-enqueue inserts nothing for a missing profile.
- Daemon test: auto jobs follow reordered profile ids and are consumed in that
  order for a newly indexed photo.
- UI tests: `moved_profile_order` at the top, middle, bottom, and empty list.
- Demo verification: move a profile, restart, confirm order; duplicate and edit
  the copy; open delete, cancel, confirm; try to delete the last profile; enqueue
  a pending job and confirm the block message.
- `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

## 10. Left to the builder

- Exact arrow glyph size, hover color, and spacing within the existing design
  tokens.
- Whether the duplicate/delete actions use `Button::small()` or a custom
  text-only control.
- Exact wording of the delete description and success messages.
- Test function names beyond the ones pinned in the plan.
