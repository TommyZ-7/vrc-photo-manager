# Error Detail and Retry: Design

- Date: 2026-10-23
- Issues: [#17 feat: エラー表示の詳細化と再試行導線](https://github.com/TommyZ-7/vrc-photo-manager/issues/17), [#14 feat: 圧縮失敗ジョブの再試行](https://github.com/TommyZ-7/vrc-photo-manager/issues/14)
- Status: Approved in session
- Target: `crates/core/src/{db,model,importer,scanner,logging}.rs`, `crates/daemon/src/lib.rs`, `crates/ui/src/main.rs`, docs

## 1. Intent

Failures in compression, import, scanning, and log analysis are currently hard to
understand from the UI. Compression stores the reason in
`compression_jobs.error`, but the queue only shows status and progress. Import
stops at the first failing file and shows a one-line header message. Scan and log
analysis failures exist only on the daemon's stderr, and no log file is written.

This change makes every failure readable and gives the user a retry path where a
retry is meaningful: compression jobs, failed import files, and daemon
scan/log/link operations. It also writes UI and daemon logs to a known folder and
adds an "open log folder" action. The issue explicitly includes compression job
retry, so this change also completes the acceptance criteria of #14.

## 2. Goals

- Show the stored reason for every failed compression job, with the photo file
  name, retry count, and a per-job retry button.
- Add "retry all failed jobs" with a per-job retry cap to prevent endless loops.
- Continue importing after a per-file failure, record which source file failed
  and why, and let the user retry only the retryable failures.
- Record daemon scan, incremental-index, log-analysis, and link failures in the
  shared database so the UI can display them.
- Let the UI ask the daemon to re-run scan, log analysis, or linking through the
  existing SQLite shared state (no IPC).
- Persist UI and daemon logs under the user data directory and expose the log
  folder path plus an "open folder" button in the UI.
- Audit swallowed errors on user-visible and lifecycle paths and leave at least
  a `tracing` record where no UI message is appropriate.

## 3. Non-goals

- Implementing the real-time IPC in #18 (sockets, progress events, pause/quit
  commands). This design uses a small command queue in SQLite and keeps the ADR
  decision intact.
- Import folder selection, changing the destination per import, or a progress
  bar (#16). The import result panel only reports the current file-selection
  flow.
- Automatic retry of compression jobs. Retry remains a user action.
- A general notification center, error history, or filtering older resolved
  errors. The UI shows only current unresolved daemon errors and the most recent
  import result.
- Changing compression output naming, replace semantics, or profile behaviour.
- Changing the log parser's event format or retaining raw VRChat logs.

## 4. Current behavior and constraints

- `compression_jobs` has no retry counter. A failed job can only be replaced by
  enqueuing a new job from the photo inspector, which loses the original job
  context and has no loop guard.
- `Db::next_pending_job` atomically moves one `pending` job to `processing`.
  Retrying is safe because the worker only sees `pending` jobs.
- `process_job` updates a job to `failed` with `error.to_string()` in both the
  daemon loop and `compress_pending`; the original photo is not modified before
  a successful compression and replace.
- `importer::import_files` uses `?` inside the per-file loop, so one failing file
  aborts the whole batch and loses the source path. `ImportResult` has only
  `imported` and `skipped`.
- `scanner::scan_and_index` continues after a per-photo upsert error but only
  logs it, and `ScanReport` has no failure list. `scan_root` returns a hard error
  for a missing root.
- The daemon and UI share SQLite (WAL) and the daemon heartbeat. The UI polls
  every three seconds. There is no command channel; ADR-0002 explicitly leaves
  immediate commands to a future IPC change.
- The daemon currently logs to stderr only. When the UI spawns it, stderr is not
  captured, so those messages are effectively lost. The UI has no tracing
  subscriber at all.
- Both binaries can resolve the user data directory through
  `vrc_photo_core::data_dir()`; thumbnails already live there.
- The UI has no GUI test harness. Pure helpers are unit tested and visual
  behaviour is checked in the demo app.

## 5. Approaches considered

### 5.1 Daemon error display and retry transport

**A. Extend the SQLite shared state (chosen).** Add a `daemon_errors` table for
current failures and a `daemon_commands` queue for UI requests. The daemon writes
errors and consumes commands; the UI reads errors and inserts commands. This
matches ADR-0002, needs no socket lifecycle, works when the daemon starts later,
and is testable with in-memory databases. The cost is a small schema addition and
up to a polling interval of latency.

**B. UI-only display and immediate retry.** The UI could scan directly and show
the result, but it cannot see daemon log-analysis failures. It would also run a
full library scan on the UI thread and risk freezing the window. This does not
satisfy the issue's log-analysis requirement.

**C. Implement #18 IPC now.** A Unix socket / named pipe would give immediate
commands and events, but it is a much larger change with reconnect, protocol, and
packaging work. #17 is P1 and #18 is P2; blocking this issue on #18 is not
justified.

### 5.2 Compression retry storage

**A. Add `compression_jobs.retry_count` (chosen).** The counter is stored next to
the job, is reset only by a new job, and lets both the database and UI enforce the
cap. Retrying keeps the original `photo_id`, `profile_id`, and `policy`.

**B. Re-enqueue a new job with the same photo/profile.** This hides the history,
loses the original failure reason, and makes "retry all failed" duplicate jobs
when clicked repeatedly.

**C. Parse the error text or keep retry state in memory.** Fragile and does not
survive a restart.

### 5.3 UI surface

**A. Global alert bars plus screen-local detail (chosen).** A compact alert row
appears above the current screen when there are failed compression jobs, daemon
errors, or import failures. Each row has an action that opens the screen with the
details. The detail itself lives where the operation lives: failed jobs in the
compression queue, import file failures in a result panel on Photos, and daemon
errors on the Monitor screen. Long reasons are not forced into the existing
one-line header message.

**B. Only expand the existing header message.** The smallest change, but the
header truncates and disappears when another message arrives. It cannot show a
file list or per-job actions.

**C. A modal error dialog.** Good for a single blocking failure, but these errors
are background/history state that the user may want to inspect while continuing
other work.

## 6. Design

### 6.1 Schema V4

`Db::migrate` gains a version 4 step:

```sql
ALTER TABLE compression_jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS daemon_errors (
    operation TEXT PRIMARY KEY,
    message TEXT NOT NULL,
    detail TEXT,
    occurred_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS daemon_commands (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    command TEXT NOT NULL,
    created_at TEXT NOT NULL
);
```

- `retry_count` is the number of user retries already performed, not the number
  of failures. New jobs start at 0.
- `daemon_errors` keeps one current row per operation; a new failure replaces the
  message/detail and increments `attempts`. A success deletes the row.
- `daemon_commands` is a FIFO request queue. Commands are deduplicated by name so
  repeated clicks cannot build an unbounded queue.
- Migration tests cover a fresh database and an existing version-3 database.

### 6.2 Core models and database API

`crates/core/src/model.rs`:

```rust
pub const MAX_JOB_RETRIES: i64 = 3;

pub struct CompressionJob {
    // existing fields...
    pub retry_count: i64,
}

pub enum DaemonOperation { Scan, Log, Link }

impl DaemonOperation {
    pub fn as_str(self) -> &'static str; // "scan" | "log" | "link"
    pub fn parse(value: &str) -> Option<Self>;
    pub fn retry_command(self) -> DaemonCommand;
}

pub enum DaemonCommand { Rescan, Relog, Relink }

impl DaemonCommand {
    pub fn as_str(self) -> &'static str; // "rescan" | "relog" | "relink"
    pub fn parse(value: &str) -> Option<Self>;
}

pub struct DaemonError {
    pub operation: DaemonOperation,
    pub message: String,
    pub detail: Option<String>,
    pub occurred_at: String,
    pub attempts: i64,
}
```

`crates/core/src/db.rs`:

```rust
impl Db {
    pub fn retry_job(&self, id: &str) -> Result<bool>;
    pub fn retry_failed_jobs(&self) -> Result<usize>;
    pub fn list_jobs_by_status(&self, status: QueueStatus, limit: i64) -> Result<Vec<CompressionJob>>;

    pub fn record_daemon_error(
        &self,
        operation: DaemonOperation,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()>;
    pub fn clear_daemon_error(&self, operation: DaemonOperation) -> Result<()>;
    pub fn daemon_errors(&self) -> Result<Vec<DaemonError>>;

    /// Inserts a command unless the same command is already queued.
    /// Returns false when it was already present.
    pub fn request_daemon_command(&self, command: DaemonCommand) -> Result<bool>;

    /// Atomically removes and returns every queued command.
    pub fn take_daemon_commands(&self) -> Result<Vec<DaemonCommand>>;
}
```

- `retry_job` runs one conditional update:
  `status = 'pending', progress = 0, error = NULL, started_at = NULL,
  finished_at = NULL, retry_count = retry_count + 1` where the id exists,
  the status is `failed`, and `retry_count < MAX_JOB_RETRIES`. It returns whether
  a row changed.
- `retry_failed_jobs` applies the same update to every eligible failed job and
  returns the number of changed rows. Jobs at the cap are left untouched.
- `record_daemon_error` is an upsert that replaces the message/detail/time and
  increments `attempts`.
- `take_daemon_commands` deletes the rows and returns their parsed commands in
  insertion order. Unknown strings are ignored and removed.
- `list_jobs_by_status` backs the failed-job section (latest 100) while
  `count_jobs(QueueStatus::Failed)` backs the badge and alert count. The badge
  and "retry all" therefore stay correct even when more than 100 failures exist.
- `CompressionJob` queries (`next_pending_job`, `list_jobs`, `list_jobs_by_status`)
  include `retry_count`; `row_to_job` reads it.

### 6.3 Compression retry semantics

- A failed job's photo, profile, and policy are never modified by a retry.
- A retry resets progress, error, start, and finish times. The daemon worker
  sees it on its next 400 ms poll.
- Each job can be retried three times. The fourth attempt is refused with a
  message telling the user to check the profile, output folder, or disk space.
  Re-enqueuing the photo from the inspector creates a new job with a fresh count.
- If the profile was deleted after the failure, the worker fails the retried job
  again with "compression profile no longer exists"; the UI shows that reason.
- If the photo is missing, the worker fails with "photo no longer exists"; this
  is also shown.
- "Retry all failed" skips jobs at the cap and returns the number queued.

### 6.4 Import failures and retry

`crates/core/src/importer.rs`:

```rust
pub enum ImportIssueKind {
    Failed,
    PartialMove,
}

pub struct ImportIssue {
    pub source: PathBuf,
    pub target: Option<PathBuf>,
    pub message: String,
    pub kind: ImportIssueKind,
    pub retryable: bool,
}

pub struct ImportResult {
    pub imported: Vec<PathBuf>,
    pub skipped: usize,
    pub issues: Vec<ImportIssue>,
}
```

- `import_files` still returns a hard error only for a destination-level failure
  (`create_dir_all`).
- Each source is handled independently:
  - a missing source is a `Failed`, `retryable: false` issue ("ファイルが見つかりません");
  - a non-image is counted as `skipped`;
  - a copy error is a `Failed`, `retryable: true` issue;
  - a cross-device move whose copy succeeded but whose source deletion failed is a
    `PartialMove` issue with `retryable: false`. The target **is** added to
    `imported`, because it exists in the destination and the destination scan will
    index it. The issue carries the source, target, and removal error so the panel
    can explain that the copy succeeded but the original remains.
- The UI counts only `Failed` issues as failures; `PartialMove` issues appear in a
  warning section. It retries only `Failed` issues with `retryable: true`, using
  the same destination and mode as the original operation. It then re-indexes the
  destination and merges the new results into the report.
- There is no import retry cap. Each retry is an explicit user action and the
  report keeps showing the reason; an automatic loop is impossible.

### 6.5 Scan failures

`crates/core/src/scanner.rs`:

```rust
pub struct ScanFailure {
    pub path: PathBuf,
    pub message: String,
}

pub struct ScanReport {
    // existing fields...
    pub failures: Vec<ScanFailure>,
}
```

- `walk` records unreadable directories and file-type errors as failures instead
  of only logging them.
- `scan_one_detailed(root, path)` returns:
  - `None` for a path that is not a candidate source photo;
  - `Some(Err(ScanFailure))` when a candidate cannot be read, parsed, or has no
    image dimensions;
  - `Some(Ok(ScannedPhoto))` otherwise.
  `scan_one` remains a thin wrapper for existing callers.
- `scan_and_index` appends per-photo upsert failures to `ScanReport.failures` and
  continues. It does not abort the batch.
- `scan_photos` in the daemon handles each root independently. A missing or
  unreadable root becomes a failure and the other roots are still scanned.
- **Missing-photo safety:** when a root scan has any failure, the daemon skips
  that root's `mark_missing` diff entirely. An unreadable subdirectory or a
  temporarily unmounted root must never mark the whole branch missing. If the root
  scan succeeds with no failures, the existing diff runs unchanged. Skipping the
  diff for one root does not affect other roots.
- `ScanReport.failures` is the source for daemon `scan` errors and for the import
  result panel's "索引できなかったファイル" section.

### 6.6 Daemon error reporting and commands

`Daemon` gains an in-memory cache of the last recorded error signature per
operation, loaded from `daemon_errors` at the start of `run`/`run_once`.

- `report_daemon_error(operation, message, detail)` writes to the database only
  when the signature (message + detail) differs from the cache. This avoids a
  write every 750 ms for a persistent log error.
- `clear_daemon_error(operation)` deletes the row when the cached signature
  exists, so a recovered operation disappears from the UI on the next poll.
- `attempts` therefore counts distinct reports (new message/detail), not every
  polling tick. The UI labels it as "検出回数".

Operations:

| Operation | Recorded when | Cleared when | Retry command |
|---|---|---|---|
| `scan` | full-scan root/read/upsert failures, or an incremental watcher indexing error | a full `scan_photos` completes with no failures | `rescan` |
| `log` | log folder unreadable, log file read fails, or per-line ingestion fails | a later log pass processes at least one new line with no errors and no skipped lines | `relog` |
| `link` | `relink_unlinked` returns an error | a link pass succeeds | `relink` |

- Incremental indexing success does **not** clear a `scan` error; only a full
  rescan (manual retry or the next daemon start) can prove that every root is
  readable again.
- A pass with no new log lines leaves an existing `log` error visible. It clears
  only after a later pass processes real log data successfully, so a skipped-line
  error is not erased by the next empty poll or by later lines in the same pass.

Fatal-versus-recoverable policy:

- Missing roots, unreadable directories, a missing log folder, an unreadable log
  file, and per-line ingestion errors are recoverable: they are recorded and
  swallowed, and `scan_photos`/`ingest_logs` return `Ok` so `run()` and
  `run_once()` keep running. The periodic loop catches any remaining error,
  records it, traces it, and continues instead of exiting.
- Database-open/migration/settings-save failures remain fatal at startup. A
  database failure inside a periodic operation is traced and recorded when
  possible, then the loop continues; there is nothing useful the UI can do with a
  daemon that exits silently.
- `--once` exits 0 for recoverable data-source failures; the UI and logs carry the
  reason. It exits non-zero only for a fatal database/setup error.
- The compression worker and `compress_pending` continue to store the job error;
  they do not create `daemon_errors` rows because the job is the user-visible
  record.

Daemon loop changes:

- `poll_commands()` runs once per loop before the periodic work. It takes all
  queued commands, reloads `AppSettings` from the database, rebuilds the file
  watchers for the current roots, and executes each command. Reloading settings
  and rebuilding watchers make "fix the path, then retry" work without a daemon
  restart; the settings screen text is updated accordingly.
- `rebuild_watchers()` drops the old watchers/channel and calls the existing watch
  setup for the reloaded roots. A watch error is recorded as a `scan` error.
- `rescan` calls `scan_photos`, then `link_unlinked`, refreshes status, and lets
  the existing thumbnail channel handle new ids.
- `relog` calls `ingest_logs` and then `link_unlinked`.
- `relink` calls `link_unlinked`.
- A command failure is recorded under its operation; the command is consumed and
  the user can click retry again.
- `run_once()` processes queued commands before its normal pass, so `--once` also
  honors a pending UI request. `compress_pending` is reached through the same
  `--once` path in `main.rs` and therefore sees the same command handling.
- `latest_log_file` returns `Result<Option<PathBuf>>` so a permission or I/O
  error is not silently treated as "no log file". A configured folder that does
  not exist is recorded as a `log` error. A readable folder with no matching log
  file is not an error.
- `read_new_lines` returns each complete line with its end byte offset.
  `log_imports.last_offset` advances after each successfully ingested line. On a
  per-line error the daemon stops the batch with the offset still before the
  failing line, records the first reason, and retries that line on the next poll.
  After three consecutive failures at the same offset, that one line is skipped
  and recorded in the detail so a poison line cannot block all future log
  analysis. The pass that skips a line never clears the error; a later pass with
  at least one clean line and no skip clears it.

### 6.7 UI

State additions on `App`:

```rust
daemon_errors: Vec<DaemonError>,
failed_jobs: Vec<CompressionJob>,
failed_job_count: i64,
import_report: Option<ImportReportView>,

struct ImportReportView {
    mode: ImportMode,
    dest: PathBuf,
    imported: usize,
    skipped: usize,
    issues: Vec<ImportIssue>,
    scan_failures: Vec<ScanFailure>,
    scan_error: Option<String>,
}
```

`reload_aux` and `refresh_live` load `db.daemon_errors()`,
`db.list_jobs_by_status(QueueStatus::Failed, 100)`, and
`db.count_jobs(QueueStatus::Failed)`.

**Global alert bars.** `content()` wraps every screen in a vertical container and
prepends zero or more compact alert rows:

- failed compression jobs: "圧縮に N 件の失敗があります" → "圧縮を開く"
  (N comes from `failed_job_count`, not the loaded slice);
- daemon errors: "デーモンで N 件の処理エラーがあります" → "ログ監視を開く";
- import `Failed` issues in the current report: "取り込みで N 件失敗しました" →
  "写真を開く". `PartialMove` issues alone do not show a failure alert; they are
  visible in the import panel as warnings.

The rows use the existing red/danger palette, are not dismissible, and disappear
when the underlying state is resolved or the import report is closed. Detail
text stays on the target screen so the bar never truncates the actual reason.

**Compression queue.** `queue_panel` becomes a `&mut self` method with `cx`:

- The header shows the recent job count and a red "失敗 N" badge from
  `failed_job_count` when N > 0.
- Failed jobs (`failed_jobs`, latest 100) are listed in their own section before
  the recent non-failed jobs, so a failure is not hidden by the 8-row preview
  limit. If `failed_job_count > 100`, the section says that only the latest 100
  are shown; "retry all" still covers every eligible job in the database.
- Each failed row shows the photo file name (looked up by `photo_id`), profile
  name if available, the full error text (wrapped), and `再試行 n/3`.
- A "再試行" button appears while `retry_count < MAX_JOB_RETRIES`; at the cap the
  row says the cap was reached and asks the user to check the profile/output.
- The queue header gets "失敗をすべて再試行" when at least one failed job exists;
  the database update itself skips jobs at the cap.
- Success/failure of a retry is reported in the existing header message; the
  failed list, count, and recent jobs are reloaded immediately.

**Import result panel.** `photos_screen` renders the panel below the Photos
header while `import_report` is present:

- title with mode (コピー / 移動) and destination;
- counts: imported / skipped / failed (`Failed` issues only);
- failed files list: source path, reason, and only retryable entries get a retry
  action;
- partial-move warnings: source, target, and "コピーは完了しましたが元ファイルを
  削除できません" (not counted as failed, not retried);
- scan failures: path and reason;
- a hard scan error if the destination could not be indexed;
- "失敗分を再試行" (only when retryable `Failed` issues exist) and "閉じる".

`import_dialog` is refactored into "run import" and "index destination" helpers so
the initial run and retry share the same scan/link/thumbnail/reload path.
Missing `managed_dir` registration and settings-save failures are logged and
shown instead of ignored.

Saving paths in Settings inserts a `Rescan` command after the settings are
written. The message becomes "保存しました。デーモンに再スキャンを要求しました
（停止中なら起動後に実行）". If the insert fails, the save still succeeds and the
message explains that a daemon restart is required.

**Monitor screen.** A "処理エラー" card lists each `DaemonError`:

- operation label (写真スキャン / ログ解析 / 写真の紐付け);
- message in red, detail in a wrapped monospace block;
- `検出回数` and timestamp (`MM/DD HH:MM` local time, raw fallback);
- "再試行" button that inserts the operation's command. If the daemon is
  stopped, the command stays queued and the UI says it will run after restart.

A "ログ" card shows the log folder, the two file names, and "ログフォルダを開く".
The Settings screen gets the same log card. If the data directory cannot be
resolved, the card explains that file logging is unavailable and that stderr is
still used.

**Logging.** Both binaries initialize a tracing subscriber that writes to stderr
and to a file under `data_dir/logs/`:

- `vrc-photo-manager.log` (UI);
- `vrc-photo-daemon.log` (daemon);
- `RUST_LOG` keeps precedence; the default is `info,<crate>=debug`;
- at startup a log file of 10 MiB or more is renamed to `<name>.1` (replacing an
  older rotated file); no background rotation thread;
- if the directory or file cannot be opened, the binary keeps running with
  stderr only and records a warning.

### 6.8 Logging helper

`crates/core/src/logging.rs` contains path/rotation helpers only (no subscriber
init, keeping the core free of process concerns):

```rust
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub fn logs_dir() -> Option<PathBuf>;
pub fn prepare_log_file(process: &str) -> std::io::Result<Option<PathBuf>>;
pub fn prepare_log_file_in(dir: &Path, process: &str, max_bytes: u64)
    -> std::io::Result<PathBuf>;
```

Each binary has a small `logging` module that builds the subscriber with
`tracing_subscriber::fmt::writer::MakeWriterExt` (`Mutex<File>.and(stderr)`).
The UI adds `tracing` and `tracing-subscriber` dependencies; the daemon already
has them.

## 7. Failure handling

- A failed retry write (database locked, job no longer failed) leaves the job
  unchanged and shows the error; the user can try again.
- A daemon command that fails is consumed and recorded; it is not retried
  automatically to avoid a request loop. Periodic log analysis still retries new
  lines on its own every 750 ms.
- If the daemon is not running, commands remain queued. The UI says so; the next
  daemon start consumes them.
- Import retry uses the stored destination and mode. If the destination was
  deleted or is unwritable, the retry reports a hard error and keeps the previous
  report. There is no import retry cap; each attempt is explicit and visible.
- A partial move is added to `imported` and reported as a warning; it is never
  retried automatically, because the target already exists.
- A log line that fails three times at the same offset is skipped and recorded in
  the `log` detail. Later lines continue. The error remains visible until a line
  is processed successfully, so a poison line cannot silently disappear.
- If rebuilding watchers fails after a settings retry, the scan error is recorded
  and the old watchers are left stopped; the next rescan/restart re-attempts the
  setup.
- Log initialization failure is non-fatal. The UI still shows the intended path
  and tells the user that file logging is unavailable.

## 8. Swallowed-error audit

The implementation replaces silent `let _ =` / `unwrap_or_default()` on the
following user-visible or lifecycle paths with `tracing::warn!`/`debug!` and, where
the user can act, a UI message:

| Area | Current behavior | New behavior |
|---|---|---|
| UI library index/page query | `eprintln!` or silent previous snapshot | `tracing::error!`; existing red banner and retry stay |
| UI settings save (sidebar, motion, paths, switches) | `let _ =` | tracing warn; path save also shows a message |
| UI profile save/edit/duplicate/reload | `let _ =` / `unwrap_or_default` | tracing warn; header message on write failure |
| UI favorite, trash mark-missing, tags, notes | `let _ =` / silent `ok()` | tracing warn; existing header message |
| UI thumbnails / preview decode | `Option` ignores reason | tracing debug/warn with photo id and path |
| UI import scan/link/thumbnail | `unwrap_or_default` / `let _ =` | import report / tracing |
| Daemon thumbnail generation | `let _` | tracing debug/warn with photo id |
| Daemon `mark_missing` | `let _` | tracing warn with id |
| Daemon heartbeat `set_setting` | `let _` | tracing warn |
| Daemon log tail `finish` | `let _` on shutdown | tracing warn |
| Daemon job failure update | `let _` | tracing error |
| Daemon watcher/thumbnail channel sends | `let _` during shutdown | tracing debug |
| Core temp-file cleanup after a failed rename | `let _` | tracing debug (the original error is still returned) |
| SQLite `journal_mode`/`synchronous` pragmas | ignored intentionally | keep, with a comment explaining in-memory databases |

Ignored `send` results after a worker has intentionally stopped are documented as
expected; they do not become errors.

## 9. Risks

- The SQLite schema becomes part of the UI/daemon API. Both binaries are shipped
  together, so version 4 is safe; an old daemon with a new UI would not know the
  new tables, but it already cannot satisfy the feature.
- A persistent daemon error writes one row and then stays cached; `attempts` does
  not grow while the message is unchanged. This is intentional to avoid write
  churn on every polling tick.
- A user can queue a retry while the daemon is busy; it runs after the current
  loop iteration, not immediately.
- Error text may contain absolute paths. This already applies to stderr and the
  job error column; the log folder is inside the user data directory.
- Rotating only at startup means a single long-running session can exceed
  10 MiB. This is acceptable for the current logging volume and avoids a
  background thread.
- Skipping the missing-photo diff for a root with any scan failure can leave
  genuinely deleted photos marked present until a clean scan. This is the safe
  trade-off: a transient permission or mount failure must not hide or "delete" a
  whole branch of the library.
- Rebuilding watchers on every rescan briefly closes the old watchers, so an
  event that arrives during the rebuild can be missed. The full rescan that
  follows the rebuild re-indexes the current state, covering the gap.
- A user can retry an import failure indefinitely; each attempt is explicit and
  the report shows the same reason. There is no automatic loop.
- A per-line log error can be skipped after three attempts, so that one event is
  not recorded. The detail states that the line was skipped, and raw VRChat logs
  are not retained by design.

## 10. Verification

Core tests:

- migration from version 3 adds `retry_count`, `daemon_errors`, and
  `daemon_commands`;
- `retry_job` only changes failed jobs, resets fields, increments the counter,
  and refuses after three retries;
- `retry_failed_jobs` returns the eligible count and leaves capped jobs alone;
- `list_jobs_by_status` returns only the requested status and respects the limit;
- daemon error upsert increments attempts, clear removes the row, and ordering is
  deterministic;
- command requests deduplicate and `take_daemon_commands` drains them;
- `import_files` imports a good file and records a missing source as a `Failed`
  issue;
- importer records a partial move as `PartialMove`, includes the target in
  `imported`, and marks it non-retryable (using an injected/mock failure path
  where practical);
- `scan_root_report` and `scan_and_index` record unreadable/upsert failures and
  continue;
- `read_new_lines` reports per-line end offsets, and the poison-line attempt
  helper skips only after three failures;
- `logging::prepare_log_file_in` creates the folder, appends, and rotates at the
  configured limit.

Daemon tests:

- a missing photo root records a `scan` error; creating the folder and processing
  a `rescan` command clears it and indexes the photo;
- a root scan with a failure does not mark unseen photos under that root missing;
- a configured but invalid log folder records a `log` error; a valid folder and
  `relog` command clear it;
- a failed job manually marked in the database can be retried, compressed, and
  completed end to end;
- command processing reloads settings changed in the database and rebuilds
  watchers for the new root;
- `run_once` consumes a queued command.

UI tests:

- daemon operation labels and retry-command mapping;
- import summary counts, `PartialMove` filtering, and retryable `Failed` filtering;
- timestamp formatting with a valid RFC3339 value and a raw fallback;
- compression retry eligibility at 0, 2, and 3 retries.

Manual verification in the demo app:

- seed a failed job and a daemon error row in the demo database, confirm the alert
  bars, queue detail, error card, and retry buttons;
- import a mix of a valid file and a missing path, confirm the file-level report
  and retry;
- open the log folder and confirm both log files exist;
- run `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

## 11. Left to the builder

- Exact alert-bar spacing, icon treatment, and whether multiple rows use borders
  or a single grouped panel.
- Number of detail lines shown before truncating a daemon detail block.
- Whether the compression queue shows the profile name in addition to the file
  name.
- Exact Japanese wording for retry-cap, import partial-move, and log-unavailable
  messages.
- Test function names beyond the behaviours pinned above.
