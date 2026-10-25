# Import UX: Design

- Date: 2026-10-25
- Issue: [#16 feat: 取り込みUX改善（フォルダ選択・保存先変更・進捗）](https://github.com/TommyZ-7/vrc-photo-manager/issues/16)
- Status: Approved in session
- Target: `crates/core/src/importer.rs`, `crates/ui/src/main.rs`, `docs/features.md`, `docs/ui.md`

## 1. Intent

Import currently starts from two header buttons that immediately open an
OS file picker. The destination is always the `managed_dir` from settings,
the import runs synchronously on the UI thread, and the result panel only
reports counts and per-file failures. There is no way to import a folder,
no progress while 100+ files are copied, and no record of which files were
renamed to resolve name collisions.

This change adds folder import, visible progress with cancellation, and a
useful completion report while keeping the destination fixed to settings.
Per-operation destination selection was removed from scope by explicit
user decision on 2026-10-25.

## 2. Goals

- Import every supported image under a selected folder, recursively.
- Show live progress (attempted/total, imported, skipped, failed) during
  the import, and keep the UI responsive.
- Let the user cancel an import. Files already imported stay in place.
- Show the final counts, per-file failures, and collision renames.
- Make the remaining files after a cancellation resumable from the result
  panel, so re-selecting the same copy source does not create numbered
  duplicates.
- Keep the existing retry of retryable failures working with the new
  fields.

## 3. Non-goals

- Changing the destination per import. The destination is the settings
  `managed_dir` (or the default `imported/` folder when unset).
- Persisting a per-import destination or remembering the last picked
  source folder.
- Preserving the source folder hierarchy in the destination. All imported
  files still land flat in the destination and collisions get `-1`, `-2`.
- Drag-and-drop import, import from the clipboard, or watching the source
  folder for new files.
- Compression as part of the import operation. The modal only copies or
  moves files. Auto-compression remains a daemon feature driven by
  newly-indexed photos, and manual compression remains a separate action.
  This design adds no import-time compression controls or job enqueue.
- Changing when the daemon enqueues auto-compression jobs. The UI indexes
  imported photos before asking the daemon to rescan, so an imported
  photo can miss the daemon's `new_ids` auto-compression path. That is
  existing behavior and is tracked separately in
  [#97](https://github.com/TommyZ-7/vrc-photo-manager/issues/97).
- Resuming an import after the result panel is closed.
- The remaining #56 edge cases: scrolling for the partial-move and scan
  failure sections, thumbnail failure visibility, and `photo_by_id` error
  tracing. Only the retry-merge test is covered here because the merge
  changes.
- Duplicate suppression by content at import time. Hashing and duplicate
  badges continue to run during indexing.
- A progress UI for the folder walk or for indexing after the copy.

## 4. Current behavior and constraints

- `importer::import_files` is synchronous, loops over files, and only
  returns after the whole batch. It has no progress or cancellation
  hook. The UI calls it on the main thread from `run_import`.
- `ImportResult` carries `imported: Vec<PathBuf>`, `skipped`, and
  `issues`. It does not say which targets were renamed, and there is no
  `processed` counter.
- `unique_target` already appends `-N` until a free name is found. That
  behavior stays.
- `ImportReportView` feeds the panel and the global alert bar. It already
  separates `Failed` and `PartialMove` issues and holds scan failures plus
  a hard scan error string.
- `retry_import` re-imports only `Failed` + `retryable` sources, keeps
  non-retryable failures and partial moves, and merges counts.
- `index_import_destination` saves the destination into `photo_roots`,
  registers it as a `managed` root, scans it, generates thumbnails for new
  photos, relinks, and asks the daemon for a rescan. It is synchronous
  and runs on the UI thread after the copy.
- `scanner::walk` already skips symlinks and a directory literally named
  `compressed`; the folder import must follow the same rules to avoid
  importing compressed derivatives or looping on symlinks.
- The manual compression dialog is the established modal pattern: a full
  screen scrim, a centered panel, `manual_option_row` choices, and a
  footer with cancel/confirm. `gpui-kit 0.8` has no menu/popover
  component, so a custom popover would be new UI code.
- UI tests are mostly pure unit tests next to `main.rs`; core tests use
  `tempfile` and image fixtures.

## 5. Approaches

### A. Import modal (chosen)

The header buttons open a modal that chooses the source (files or folder),
shows the fixed destination, and starts the import. The same modal turns
into the progress/cancel view. On completion it closes and the existing
result panel shows the outcome, including renames and a resume action.

Trade-offs: one extra click before the file picker; in exchange the
source choice, destination, progress, and cancellation each have an
obvious home, and the implementation reuses the compression modal pattern.

### B. Header-direct with a folder button

The file picker opens immediately and a new folder button sits beside it;
a progress bar and cancel action live below the header.

Trade-offs: fewest clicks, but the header grows to two buttons per mode
(or a split button), there is no pre-import summary, and progress
competes with the existing result panel.

### C. Source popover

A small custom menu offers "files" or "folder", and progress renders in
the library header.

Trade-offs: visually lighter, but `gpui-kit` has no popover component and
the section would be custom, outside the established modal pattern.

## 6. Decisions

- **Destination**: fixed to `settings.import_dir()`. The modal only
  displays it and points at Settings for changes. The destination is
  snapshotted when the modal opens; if the setting changes while the
  modal is open, the snapshot is still used for that run.
- **Folder import**: always recursive. Skip directories named
  `compressed` (same as the scanner), skip symlinks, and skip the import
  destination subtree if it is inside the selected folder. Sort collected
  files by path so numbering and progress are deterministic.
- **Flat destination**: subfolders are not recreated. Collisions use the
  existing numeric suffix and are reported as renames.
- **Cancel**: stop before the next file. The file currently being copied
  or moved finishes (the copy is atomic and never leaves a partial file).
  Already imported files stay; there is no rollback. The report shows the
  unprocessed count and a "残りを再開" action while the panel stays open.
- **No recovery after closing the panel**: closing the result panel
  discards the remaining list. Copying the same folder again would
  re-import finished files as `-N` duplicates, so the docs say to resume
  from the panel instead.
- **One execution path**: initial import, retry, and resume all run
  through the same background/observed/cancellable pipeline and the same
  modal progress view. Retry and resume open the modal directly in the
  running state, with a run-local progress bar (`0 / N`). This keeps a
  60-file resume or a large retry from freezing the UI.
- **Progress scope**: progress covers the copy/move loop only. Folder
  walking happens synchronously right after the folder picker returns;
  indexing and thumbnail generation stay synchronous after the copy, as
  today.
- **Indexing**: only run `index_import_destination` when the finished run
  imported at least one file. The gate is the current run's result, not
  the cumulative report. A retry or resume that imports nothing keeps the
  previous scan failures; one that imports something clears them and
  records the fresh scan result. Cancelling before any file therefore
  does not register or scan the destination.
- **Retry vs resume**: retry re-runs retryable failures and does not
  advance the original progress counter. Resume re-runs the unprocessed
  sources and does advance it. Both merge renames and failures into the
  same report. Cancelling a retry leaves the unattempted retryable
  failures in the report so retry can run again; cancelling a resume puts
  the unattempted sources back into `remaining`.

## 7. Core API

All changes are in `crates/core/src/importer.rs`.

```rust
pub struct ImportRename {
    pub source: PathBuf,
    pub target: PathBuf,
}

pub struct ImportResult {
    pub imported: Vec<PathBuf>,
    pub skipped: usize,
    pub issues: Vec<ImportIssue>,
    pub renamed: Vec<ImportRename>,
    /// Sources attempted in this run. Includes skipped files and failures.
    pub processed: usize,
    /// True only when cancellation was observed before an unprocessed file.
    pub cancelled: bool,
}

pub struct ImportProgress {
    pub processed: usize,
    pub total: usize,
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
    pub partial: usize,
    /// Last source handled by this run; None on the initial event.
    pub last_source: Option<PathBuf>,
}
```

- `ImportResult` derives `Default`; new fields default to empty/zero/false.
- `renamed` gets an entry for every successful import whose target file
  name differs from the source file name. This covers collisions with
  existing files and collisions between sources in the same batch.
- `import_files(files, dest, mode)` keeps its signature and delegates to
  the observed version with no cancel flag and a no-op observer.
- New entry point:

  ```rust
  pub fn import_files_observed<F>(
      files: &[PathBuf],
      dest: &Path,
      mode: ImportMode,
      cancel: Option<&AtomicBool>,
      observe: F,
  ) -> Result<ImportResult>
  where
      F: FnMut(ImportProgress);
  ```

- The observer is called once before the first file (`processed = 0`,
  `last_source = None`), then once after every attempted file.
- Cancellation is checked at the top of each iteration. If set, the run
  stops without processing the next source, sets `cancelled = true`, and
  returns normally. A cluster of already-processed files is not rolled
  back.
- Destination creation remains a hard error before the loop, as today.
  Per-file problems remain inside `issues`.
- New folder entry point:

  ```rust
  pub struct FolderScan {
      pub files: Vec<PathBuf>,
      /// Unreadable directories, expressed as non-retryable import failures.
      pub issues: Vec<ImportIssue>,
  }

  pub fn collect_folder_sources(folder: &Path, exclude: Option<&Path>) -> Result<FolderScan>;
  ```

  - `folder` must be an existing directory; otherwise return
    `CoreError::Invalid`.
  - Walk recursively, sorted by path. Skip symlinks, directories named
    `compressed`, and the `exclude` subtree (canonicalized when it
    exists).
  - A nested `read_dir`/entry failure becomes an `ImportIssue` with
    `kind = Failed`, `retryable = false`, `source = <directory>` and a
    Japanese message, so the existing failed-file section and counts can
    show it.
  - Extension matching uses `photo_filename::is_source_photo`.
  - An empty result is not an error; the UI decides how to present it.

## 8. UI

### 8.1 State and flow

New app state:

```rust
enum ImportSource {
    Files(Vec<PathBuf>),
    Folder {
        folder: PathBuf,
        files: Vec<PathBuf>,
        issues: Vec<ImportIssue>,
    },
}

enum ImportDialogOrigin {
    New,
    Retry {
        report: ImportReportView,
        sources: Vec<PathBuf>,
    },
    Resume {
        report: ImportReportView,
        sources: Vec<PathBuf>,
    },
}

enum ImportDialogPhase {
    Ready,
    Running {
        progress: ImportProgress,
        cancel: Arc<AtomicBool>,
        cancel_requested: bool,
    },
}

struct ImportDialogState {
    origin: ImportDialogOrigin,
    mode: ImportMode,
    dest: PathBuf,
    source: Option<ImportSource>, // used by New only
    error: Option<String>,
    phase: ImportDialogPhase,
}
```

- `open_import_dialog(mode)` snapshots the destination and opens the
  modal in `Ready` with `origin = New`.
- `open_retry_dialog()` and `open_resume_dialog()` clone the current
  report and its retryable sources / `remaining`, then open the modal
  directly in `Running` with `origin = Retry` / `Resume`. Their panels
  show "失敗分を再試行" / "残り N 件を再開" instead of the source
  buttons.
- File source: `rfd::FileDialog::new().add_filter("画像", ...).pick_files()`,
  as today.
- Folder source: `pick_folder()`, then `collect_folder_sources(folder,
  Some(&dest))` synchronously.
  - No files, no issues: inline error "対象画像が見つかりません" and the
    start button stays disabled.
  - Files plus issues: keep both; the issues join the report on start and
    the modal shows a warning count.
  - Hard scan error: inline error, source stays unchanged.
- Start:
  - Build the run's `Vec<PathBuf>` (source files for New, origin's
    `sources` for Retry/Resume) and an `Arc<AtomicBool>`.
  - Set `Running` with the initial progress (`processed = 0`,
    `total = files.len()`).
  - Spawn a detached background task that calls
    `import_files_observed`, forwarding `ImportProgress` and the final
    `Result<ImportResult, String>` over an `async_channel::unbounded()`
    channel. Progress sends ignore a closed receiver.
  - Spawn a foreground task that receives the channel and calls
    `this.update(...)`.
- Progress messages update `phase.progress` and notify. The final message
  dispatches on `origin`:
  - `New`: build a fresh report from the result, close the modal, and
    set the header message.
  - `Retry`: merge with `merge_retry_report`, close the modal, and update
    the message.
  - `Resume`: merge with `merge_resume_report`, close the modal, and
    update the message.
  - Run `index_import_destination` only when `result.imported` is
    non-empty. When it runs, clear the report's `scan_failures` and
    `scan_error` first; when it does not, the merged previous values
    stay.
- Import hard error:
  - `New`: return to `Ready`, keep the source selected, show
    "取り込みに失敗: …" inline. Do not open the result panel.
  - `Retry` / `Resume`: close the modal, keep the previous report
    unchanged, and set the header message ("再試行に失敗: …" /
    "再開に失敗: …"). The retry/resume action remains available.
- Cancel:
  - Ready: close the modal.
  - Running: set the flag and `cancel_requested = true`. The modal stays
    until the background task reports its final result; the button label
    becomes "キャンセル中…" and disables.
  - Scrim click follows the same rule, so a running import cannot be
    orphaned by closing the overlay.
- The modal has no close button other than the footer cancel/close.

### 8.2 Modal content

Width about 560 px, matching the manual compression panel.

Ready state:

- Title "取り込み", mode badge (コピー / 移動).
- Section "取り込み元":
  - Buttons "ファイルを選択" and "フォルダを選択".
  - Hint: "フォルダはサブフォルダも対象です。compressed フォルダは除外します。"
  - Selected summary: `N 件のファイル` or `folder path · N 件`.
  - Warning when folder scan issues exist: `N 件のフォルダを読み取れませんでした（結果に表示します）`.
  - Inline error, when any.
- Section "取り込み先": read-only path plus "設定 > ライブラリ > 取り込み先で変更できます。"
- Footer: "キャンセル" (close) and "取り込み開始" (primary, disabled
  without a non-empty source).

Running state:

- Same title and destination.
- `New` shows the selected source summary; `Retry` shows "失敗分 N 件";
  `Resume` shows "残り N 件". The progress is run-local.
- Progress bar with `import_progress_ratio`.
- Label `取り込み 42/100 · 取込 40 · スキップ 1 · 失敗 1`
  (partial moves count in 取込 and are called out in the final report).
- Last source file name, truncated.
- Footer: "キャンセル" (or "キャンセル中…" disabled).

### 8.3 Result panel

`ImportReportView` gains:

```rust
total: usize,
processed: usize,
renamed: Vec<ImportRename>,
remaining: Vec<PathBuf>,
cancelled: bool,
```

- Header badges gain "連番 N" when `renamed` is non-empty and "未処理 N"
  when `remaining` is non-empty. Failed/skip/partial/scan badges stay.
- New section "連番を付けたファイル" (max height 150 px, scrollable) with
  `source → target` rows. Show only when non-empty.
- Failed and partial sections keep their existing content and order.
- Header actions:
  - "失敗分を再試行" when retryable failures exist (existing label).
    Opens the import modal directly in the running state and is
    cancellable.
  - "残り N 件を再開" when `remaining` is non-empty (new, primary). Also
    opens the modal in the running state.
  - "閉じる" (existing).
- Summary text: `{total} 件中 {imported} 枚を取り込みました（…）` plus
  `。残り {n} 件は未処理です` when cancelled. Skipped, failed, and rename
  counts are included as today.
- Two pure merge helpers, both tested without the UI:
  - `merge_retry_report(previous, retry_sources, result)`:
    - Remove previous retryable `Failed` issues whose source is in the
      attempted prefix (`retry_sources[..result.processed]`). Unattempted
      retry sources keep their previous issue, so a cancelled retry can
      run again.
    - Append `result.issues`, `renamed`, imported, and skipped counts.
    - Keep `processed`, `total`, `remaining`, and `cancelled` unchanged.
  - `merge_resume_report(previous, result)`:
    - Slice `result.processed` sources off `remaining`.
    - Append imported/skipped/issues/renamed and add `result.processed`
      to `processed`.
    - Set `cancelled = !remaining.is_empty()`.
  - Neither helper clears scan fields. The caller clears them only when
    it actually re-indexes (the run imported at least one file).
- Resume uses the same destination and mode as the report. It does not
  re-run cancelled/unprocessed sources from the start; it only runs
  `remaining`.

## 9. Error handling

- Folder walk: top-level invalid/unreadable selection is an inline modal
  error. Nested failures become non-retryable failed issues in the
  report.
- Per-file copy/move failures and partial moves are unchanged.
- Cancellation is not an error. It never appears in the global alert bar
  because `import_issue_counts` only counts `Failed` issues.
- A closed modal with a running task is impossible: cancel is requested
  first and the modal closes only after the final message.
- If the UI is destroyed mid-run, the background task finishes the
  current file, drops the send result, and exits; the destination may
  contain the files processed so far.
- Indexing failures continue to appear as scan failures or a hard error
  in the report.

## 10. Tests

Core (`crates/core/src/importer.rs`):

- Folder collection: recursive, sorted, skips non-images, `compressed`,
  symlinks, and the destination subtree.
- Folder collection: unreadable nested directory becomes a non-retryable
  issue (Unix permission test with the existing privileged-environment
  skip pattern); selecting a file as the folder returns `Err`.
- Progress observer: initial event with `processed = 0` and the total;
  one event per attempted file with monotonic counts; skipped files
  counted in `processed`.
- Cancellation: pre-cancelled run imports nothing and reports
  `cancelled`; a mid-run cancellation triggered from the observer stops
  before the next file, leaves the first copy in place, and sets
  `processed = 1`.
- Renames: existing-target collision and same-name sources in one batch
  produce `renamed` entries with the `-1`/`-2` targets; a unique name
  produces no entry.
- `processed == files.len()` on a completed run, including skipped and
  failed files.
- Existing tests keep passing with the added fields.

UI (`crates/ui/src/main.rs`):

- `import_progress_label` and `import_progress_ratio`, including
  `total = 0`.
- `import_summary` with completed, failed, renamed, and cancelled runs.
- `merge_retry_report`: keeps non-retryable failures and partial moves,
  drops retryable failures for the attempted prefix, keeps unattempted
  retryable failures (cancelled retry), appends renames, and does not
  move the original progress counters.
- `merge_resume_report`: advances `processed`, slices `remaining`,
  clears `cancelled` when everything ran, and keeps it when the resume is
  cancelled again.
- Result panel badge/row inputs are pure helpers where practical.

Manual QA:

- Create a fixture folder with about 100 PNGs plus a nested folder,
  `compressed/`, a symlink, and a non-image. Import it as copy and confirm
  live progress, final counts, and rename rows.
- Import the same tree in move mode after copying it again; confirm
  sources disappear and partial moves remain warnings.
- Cancel around 40/100, confirm 40 files exist, the panel shows 60
  unprocessed, and "残りを再開" finishes the rest without duplicates.
- Cancel a retry and confirm the unattempted failures still offer retry.
- Confirm the destination stays fixed to Settings and changes to the
  setting are picked up by the next modal.

## 11. Documentation

- `docs/features.md` section 4: folder import rules (recursive, flat,
  `compressed`/symlink/destination exclusion), fixed destination,
  progress, cancel/resume semantics, and rename reporting.
- `docs/ui.md` section 2.2: import modal, progress/cancel, result panel
  additions, and remove "取り込みフォルダ選択" from section 9.
- Issue #16: update the body to drop per-operation destination selection
  and record the cancel/resume decision. Keep #56 open; this design only
  consumes its retry-merge test item.

## 12. Left to the builder

- Exact split between helper functions and methods, and whether the
  modal renderer lives in one method or separate ready/running helpers.
- Whether progress messages are drained in a batch per wake; not needed
  for 100 files, but allowed.
- The precise wording of inline errors and warnings beyond the quoted
  strings above.
