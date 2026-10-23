# Error Detail and Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show detailed failure reasons for compression, import, scan, and log analysis, add safe retries, and persist openable UI/daemon logs.

**Architecture:** Extend the existing SQLite shared state with `compression_jobs.retry_count`, a `daemon_errors` table, and a small `daemon_commands` request queue. The UI reads errors and queues retries; the daemon reports errors, reloads settings, rebuilds watchers, and executes commands. Compression and import retries stay in their existing subsystems. Both binaries also write tracing output to files under the user data directory.

**Tech Stack:** Rust stable (edition 2024), rusqlite/SQLite, gpui-kit 0.8, tracing/tracing-subscriber, chrono.

**Spec:** `docs/superpowers/specs/2026-10-23-error-retry-design.md`

## Global Constraints

- Work only in a `git worktree`; never create/switch branches in the main checkout. Branch: `feat/issue-17-error-retry`.
- Do not push to `main`. PR → green `CI` → merge. Never merge red.
- Commit prefixes: `feat:`, `fix:`, `docs:`, `ci:`, `chore:`.
- UI copy is Japanese and uses the existing palette/tokens in `crates/ui/src/main.rs`.
- SQLite schema changes go through a new `user_version` step; never edit an applied migration.
- Retry caps: `MAX_JOB_RETRIES = 3`. Import retries are explicit and uncapped.
- Do not add a socket/IPC layer; SQLite remains the shared state (ADR-0002).
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Every subagent runs on the parent session model; never pass a `model` argument.
- Do not create tags, publish releases, or run `gh release` commands.

## Review Focus

These are the inputs most likely to hurt a user even though the happy path works. Each one has a test in the task that owns the code.

1. An unreadable subdirectory or transient mount failure must not mark the rest of a photo root missing. (Task 6)
2. A daemon error row left from a previous process must disappear after a successful pass, not stay stale forever. (Task 6)
3. A fourth compression retry click must not reset the counter or loop forever. (Task 1)
4. A cross-device move that copies but cannot delete the source must count the target as imported, report a warning, and never retry into a duplicate. (Tasks 3, 11)
5. A poison log line must not block all later log analysis: the offset stays before it for three attempts, then that line is skipped and the error remains visible. (Task 6)

---

## Task 0: Create the isolated worktree

**Files:**
- Copy: `docs/superpowers/specs/2026-10-23-error-retry-design.md`
- Copy: `docs/superpowers/plans/2026-10-23-error-retry.md`

**Interfaces:**
- Consumes: none.
- Produces: working directory `../vrc-photo-manager-issue17` on branch `feat/issue-17-error-retry`.

- [ ] **Step 1: Confirm the main checkout is clean except for the known untracked files**

Run:
```bash
cd /home/tommyz7/work/vrc-photo-manager
git status --short --branch
```
Expected: `main...origin/main` and only `opencode.json` / local `.opencode/` overrides / the new spec and plan are untracked or modified.

- [ ] **Step 2: Create the worktree from the current `main`**

Run:
```bash
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add "../${REPO}-issue17" -b feat/issue-17-error-retry
```
Expected: `Preparing worktree (new branch 'feat/issue-17-error-retry')`.

- [ ] **Step 3: Copy both documents into the worktree**

Run:
```bash
mkdir -p ../vrc-photo-manager-issue17/docs/superpowers/specs
mkdir -p ../vrc-photo-manager-issue17/docs/superpowers/plans
cp docs/superpowers/specs/2026-10-23-error-retry-design.md \
  ../vrc-photo-manager-issue17/docs/superpowers/specs/
cp docs/superpowers/plans/2026-10-23-error-retry.md \
  ../vrc-photo-manager-issue17/docs/superpowers/plans/
```
Expected: both files exist in the worktree.

- [ ] **Step 4: Commit the design and plan**

```bash
cd ../vrc-photo-manager-issue17
git add docs/superpowers/specs/2026-10-23-error-retry-design.md \
        docs/superpowers/plans/2026-10-23-error-retry.md
git commit -m "docs: add error retry design and plan (#17)"
```

All later tasks run in `/home/tommyz7/work/vrc-photo-manager-issue17`.

---

## Task 1: Schema V4, retry counter, and compression retry API

**Files:**
- Modify: `crates/core/src/model.rs` (`CompressionJob`, `MAX_JOB_RETRIES`)
- Modify: `crates/core/src/db.rs` (`SCHEMA_V4`, migration, job queries, retry methods, tests)

**Interfaces:**
- Consumes: existing `QueueStatus`, `CompressionJob`, `now_string`.
- Produces:
  - `pub const MAX_JOB_RETRIES: i64 = 3;`
  - `CompressionJob { retry_count: i64, ... }`
  - `Db::retry_job(&self, id: &str) -> Result<bool>`
  - `Db::retry_failed_jobs(&self) -> Result<usize>`
  - `Db::list_jobs_by_status(&self, status: QueueStatus, limit: i64) -> Result<Vec<CompressionJob>>`

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/src/db.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn retry_job_resets_failed_job_and_enforces_cap() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let job = db.enqueue_job(photo, "p", "keep").unwrap();
    db.next_pending_job().unwrap();
    db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
        .unwrap();

    for expected in 1..=MAX_JOB_RETRIES {
        assert!(db.retry_job(&job).unwrap());
        let row = &db.list_jobs(1).unwrap()[0];
        assert_eq!(row.status, QueueStatus::Pending);
        assert_eq!(row.retry_count, expected);
        assert!(row.error.is_none());
        assert!(row.started_at.is_none());
        assert!(row.finished_at.is_none());
        db.next_pending_job().unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
    }

    assert!(!db.retry_job(&job).unwrap());
    assert_eq!(db.list_jobs(1).unwrap()[0].retry_count, MAX_JOB_RETRIES);
}

#[test]
fn retry_failed_jobs_returns_only_eligible_rows() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let first = db.enqueue_job(photo, "p", "keep").unwrap();
    let capped = db.enqueue_job(photo, "p", "keep").unwrap();
    let completed = db.enqueue_job(photo, "p", "keep").unwrap();
    for job in [&first, &capped, &completed] {
        db.next_pending_job().unwrap();
        db.update_job(job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
    }
    db.connection()
        .execute(
            "UPDATE compression_jobs SET retry_count = ?1 WHERE id = ?2",
            params![MAX_JOB_RETRIES, capped],
        )
        .unwrap();
    db.update_job(&completed, QueueStatus::Completed, 1.0, None, None)
        .unwrap();

    assert_eq!(db.retry_failed_jobs().unwrap(), 1);
    assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    assert_eq!(
        db.list_jobs(10)
            .unwrap()
            .into_iter()
            .find(|row| row.id == first)
            .unwrap()
            .retry_count,
        1
    );
}

#[test]
fn list_jobs_by_status_filters_and_limits() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let failed = db.enqueue_job(photo, "p", "keep").unwrap();
    db.enqueue_job(photo, "p", "keep").unwrap();
    db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
        .unwrap();

    let rows = db.list_jobs_by_status(QueueStatus::Failed, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, failed);
    assert!(db.list_jobs_by_status(QueueStatus::Pending, 10).unwrap().len() == 1);
}
```

Update the existing migration test to expect version 4 and add:

```rust
#[test]
fn migration_adds_retry_count_and_daemon_tables() {
    let db = Db::open_in_memory().unwrap();
    let version: i64 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let retry_column: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('compression_jobs') WHERE name = 'retry_count'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retry_column, 1);
    let tables: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('daemon_errors', 'daemon_commands')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 2);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core retry_job_resets_failed_job_and_enforces_cap -- --nocapture
```
Expected: FAIL because `retry_job` and `retry_count` do not exist.

- [ ] **Step 3: Implement the model fields and schema**

In `crates/core/src/model.rs` add `pub const MAX_JOB_RETRIES: i64 = 3;` and add `pub retry_count: i64` to `CompressionJob`.

In `crates/core/src/db.rs` add:

```rust
const SCHEMA_V4: &str = r#"
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
"#;
```

Add `if version < 4 { ... }` to `migrate`, recording `schema_migrations` version 4 and setting `user_version = 4`.

Update every `compression_jobs` select/returning list to include `retry_count` immediately after `policy`. Update `row_to_job` indexes accordingly. Implement `retry_job`, `retry_failed_jobs`, and `list_jobs_by_status` exactly as described in the spec.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core retry_job_resets_failed_job_and_enforces_cap
cargo test -p vrc-photo-core retry_failed_jobs_returns_only_eligible_rows
cargo test -p vrc-photo-core list_jobs_by_status_filters_and_limits
cargo test -p vrc-photo-core migration_adds_retry_count_and_daemon_tables
```
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat: add compression retry counter and job retry API (#17)"
```

---

## Task 2: Daemon error and command storage

**Files:**
- Modify: `crates/core/src/model.rs` (`DaemonOperation`, `DaemonCommand`, `DaemonError`)
- Modify: `crates/core/src/db.rs` (error/command methods and tests)

**Interfaces:**
- Consumes: `SCHEMA_V4` from Task 1.
- Produces:
  - `pub enum DaemonOperation { Scan, Log, Link }`
  - `pub enum DaemonCommand { Rescan, Relog, Relink }`
  - `pub struct DaemonError { operation, message, detail, occurred_at, attempts }`
  - `Db::record_daemon_error`, `Db::clear_daemon_error`, `Db::daemon_errors`
  - `Db::request_daemon_command`, `Db::take_daemon_commands`

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/src/db.rs` tests:

```rust
#[test]
fn daemon_error_upsert_increments_attempts_and_clear_removes_it() {
    let db = Db::open_in_memory().unwrap();
    db.record_daemon_error(DaemonOperation::Scan, "cannot scan", Some("/missing"))
        .unwrap();
    db.record_daemon_error(DaemonOperation::Scan, "still missing", None)
        .unwrap();

    let errors = db.daemon_errors().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].operation, DaemonOperation::Scan);
    assert_eq!(errors[0].message, "still missing");
    assert_eq!(errors[0].detail, None);
    assert_eq!(errors[0].attempts, 2);

    db.clear_daemon_error(DaemonOperation::Scan).unwrap();
    assert!(db.daemon_errors().unwrap().is_empty());
}

#[test]
fn daemon_commands_deduplicate_and_drain_in_order() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
    assert!(!db.request_daemon_command(DaemonCommand::Rescan).unwrap());
    assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());

    assert_eq!(
        db.take_daemon_commands().unwrap(),
        vec![DaemonCommand::Rescan, DaemonCommand::Relog]
    );
    assert!(db.take_daemon_commands().unwrap().is_empty());
    assert!(db.request_daemon_command(DaemonCommand::Relink).unwrap());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core daemon_error_upsert_increments_attempts_and_clear_removes_it
```
Expected: FAIL because the types and methods do not exist.

- [ ] **Step 3: Implement the types and methods**

Add the enums/struct to `model.rs`. Derive `Debug, Clone, Copy, PartialEq, Eq, Hash` for the enums and `Debug, Clone, PartialEq, Eq` for `DaemonError`. Implement `as_str`, `parse`, and `retry_command`:

- `Scan -> Rescan`, `Log -> Relog`, `Link -> Relink`.

Implement the DB methods with the SQL from the spec. `take_daemon_commands` must read `ORDER BY id`, delete all rows in the same transaction, parse known commands, and silently discard unknown strings. `daemon_errors` orders by `occurred_at DESC, operation`.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core daemon_error_upsert_increments_attempts_and_clear_removes_it
cargo test -p vrc-photo-core daemon_commands_deduplicate_and_drain_in_order
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat: persist daemon errors and retry commands (#17)"
```

---

## Task 3: Continue import after per-file failures

**Files:**
- Modify: `crates/core/src/importer.rs`

**Interfaces:**
- Consumes: `ImportMode`, `CoreError`, `photo_filename::is_source_photo`.
- Produces:
  - `pub enum ImportIssueKind { Failed, PartialMove }`
  - `pub struct ImportIssue { source, target, message, kind, retryable }`
  - `pub struct ImportResult { imported, skipped, issues }`

- [ ] **Step 1: Write the failing tests**

Replace the assertions in the existing tests and add:

```rust
#[test]
fn missing_source_is_reported_without_aborting_the_batch() {
    let dir = tempdir().unwrap();
    let good = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    write_png(&good);
    let missing = dir.path().join("VRChat_missing.png");
    let dest = dir.path().join("dest");

    let result = import_files(&[missing.clone(), good.clone()], &dest, ImportMode::Copy).unwrap();

    assert_eq!(result.imported.len(), 1);
    assert_eq!(result.skipped, 0);
    assert_eq!(result.issues.len(), 1);
    assert_eq!(result.issues[0].kind, ImportIssueKind::Failed);
    assert_eq!(result.issues[0].source, missing);
    assert!(!result.issues[0].retryable);
    assert!(dest.join(good.file_name().unwrap()).is_file());
}

#[cfg(unix)]
#[test]
fn move_reports_partial_move_and_keeps_the_target() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    write_png(&source);

    fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Move).unwrap();
    fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(source.is_file(), "source must remain after a failed delete");
    assert_eq!(result.imported.len(), 1);
    assert_eq!(result.issues.len(), 1);
    assert_eq!(result.issues[0].kind, ImportIssueKind::PartialMove);
    assert!(!result.issues[0].retryable);
    assert_eq!(result.issues[0].target.as_deref(), Some(result.imported[0].as_path()));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core missing_source_is_reported_without_aborting_the_batch
```
Expected: FAIL because `ImportResult` has no `issues` field.

- [ ] **Step 3: Refactor `import_files`**

Handle each source independently. Missing source is a non-retryable `Failed` issue. Non-images are `skipped`. Copy failures are retryable `Failed` issues. For move, try `rename`; if that fails, copy then remove the source. If the copy succeeds but removal fails, push the target to `imported` and add a non-retryable `PartialMove` issue containing both paths and the removal error. Keep the destination-level `create_dir_all` as a hard `Result` error.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core importer
```
Expected: all importer tests PASS, including the existing collision and non-image tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/importer.rs
git commit -m "feat: report per-file import failures and partial moves (#17)"
```

---

## Task 4: Report scan failures without aborting the batch

**Files:**
- Modify: `crates/core/src/scanner.rs`

**Interfaces:**
- Consumes: `ScannedPhoto`, `ScanReport`, `Db`.
- Produces:
  - `pub struct ScanFailure { path, message }`
  - `ScanReport { failures: Vec<ScanFailure>, ... }`
  - `pub fn scan_root_report(root: &Path) -> Result<(Vec<ScannedPhoto>, Vec<ScanFailure>)>`
  - `pub fn scan_one_detailed(root: &Path, path: &Path) -> Option<Result<ScannedPhoto, ScanFailure>>`
  - `scan_one` remains `Option<ScannedPhoto>`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/src/scanner.rs` tests:

```rust
#[test]
fn corrupt_source_is_reported_but_other_photos_are_indexed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write_png(&root.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
    std::fs::write(root.join("broken.png"), b"not an image").unwrap();

    let db = Db::open_in_memory().unwrap();
    let root_id = db.add_root(root.to_str().unwrap(), "reference").unwrap();
    let report = scan_and_index(&db, root_id, root).unwrap();

    assert_eq!(report.indexed, 1);
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].path.ends_with("broken.png"));
    assert_eq!(db.count_photos().unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let (photos, failures) = scan_root_report(dir.path()).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(photos.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].path, locked);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core corrupt_source_is_reported_but_other_photos_are_indexed
```
Expected: FAIL because `ScanReport` has no `failures` and `scan_root_report` does not exist.

- [ ] **Step 3: Implement the failure-aware scan**

Add `ScanFailure`, add `failures` to `ScanReport`, and thread a `&mut Vec<ScanFailure>` through `walk`. `walk` pushes read-dir and file-type errors instead of only logging. Implement `scan_one_detailed` for candidate photos and make `scan_one` a wrapper. `scan_root_report` returns the photos plus failures; `scan_root` delegates to it and discards failures. `scan_and_index` appends per-photo upsert failures with the absolute path and error string, then continues.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core scanner
```
Expected: all scanner tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/scanner.rs
git commit -m "feat: collect photo scan failures (#17)"
```

---

## Task 5: File logging helper

**Files:**
- Create: `crates/core/src/logging.rs`
- Modify: `crates/core/src/lib.rs` (`pub mod logging;`)

**Interfaces:**
- Consumes: `crate::data_dir()`.
- Produces:
  - `pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;`
  - `pub const UI_LOG_FILE: &str = "vrc-photo-manager.log";`
  - `pub const DAEMON_LOG_FILE: &str = "vrc-photo-daemon.log";`
  - `pub fn logs_dir() -> Option<PathBuf>`
  - `pub fn prepare_log_file(process: &str) -> std::io::Result<Option<PathBuf>>`
  - `pub fn prepare_log_file_in(dir: &Path, process: &str, max_bytes: u64) -> std::io::Result<PathBuf>`

- [ ] **Step 1: Write the failing test**

Add to `crates/core/src/logging.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prepares_appends_and_rotates_log_file() {
        let dir = tempdir().unwrap();
        let path = prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(path.file_name().unwrap(), "ui.log");

        std::fs::write(&path, b"12345678").unwrap();
        let path_again = prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(path_again, path);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(
            std::fs::read(dir.path().join("ui.log.1")).unwrap(),
            b"12345678"
        );

        std::fs::write(&path, b"abc").unwrap();
        prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
cargo test -p vrc-photo-core prepares_appends_and_rotates_log_file
```
Expected: FAIL because the module/function does not exist.

- [ ] **Step 3: Implement the helper**

`logs_dir()` returns `data_dir().map(|dir| dir.join("logs"))`. `prepare_log_file_in` creates the directory, builds `<process>.log`, renames it to `<process>.log.1` when its length is at least `max_bytes` (removing an older `.1` first), and returns the new path. `prepare_log_file` uses `MAX_LOG_BYTES` and returns `None` when the data directory cannot be resolved.

- [ ] **Step 4: Run the test to verify it passes**

Run:
```bash
cargo test -p vrc-photo-core prepares_appends_and_rotates_log_file
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/lib.rs crates/core/src/logging.rs
git commit -m "feat: add log file path and rotation helpers (#17)"
```

---

## Task 6: Daemon scan/log error reporting and safe offsets

**Files:**
- Modify: `crates/daemon/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 1–5 (`DaemonOperation`, `Db` error methods, `ScanFailure`, `prepare_log_file` not used here).
- Produces (private unless noted):
  - `Daemon.last_errors: HashMap<DaemonOperation, String>`
  - `Daemon::report_daemon_error`, `Daemon::clear_daemon_error`
  - `LogTail.failed_offset: Option<i64>`, `LogTail.failed_attempts: u8`, `LogTail::note_line_failure(i64) -> bool`
  - `read_new_lines(&Path, i64) -> Result<(Vec<(String, i64)>, i64)>`
  - `latest_log_file(&Path) -> Result<Option<PathBuf>>`

- [ ] **Step 1: Write the failing tests**

Add/update tests in `crates/daemon/src/lib.rs`:

```rust
#[test]
fn reads_only_complete_lines_with_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output_log_test.txt");
    let mut file = File::create(&path).unwrap();
    write!(file, "line1\nline2\npart").unwrap();
    drop(file);

    let (lines, offset) = read_new_lines(&path, 0).unwrap();
    assert_eq!(lines, vec![("line1".to_string(), 6), ("line2".to_string(), 12)]);
    assert_eq!(offset, 12);
}

#[test]
fn poison_line_is_skipped_only_after_three_attempts() {
    let mut tail = LogTail {
        path: PathBuf::new(),
        ingestor: LogIngestor::new("log"),
        offset: 0,
        failed_offset: None,
        failed_attempts: 0,
    };
    assert!(!tail.note_line_failure(0));
    assert!(!tail.note_line_failure(0));
    assert!(tail.note_line_failure(0));
    assert_eq!(tail.failed_attempts, 0);
    assert!(!tail.note_line_failure(10));
}

#[test]
fn stale_daemon_errors_clear_after_a_successful_run() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let db_path = dir.path().join("library.db");

    {
        let db = Db::open(&db_path).unwrap();
        db.record_daemon_error(DaemonOperation::Scan, "old scan", None)
            .unwrap();
        db.record_daemon_error(DaemonOperation::Log, "old log", None)
            .unwrap();
        db.record_daemon_error(DaemonOperation::Link, "old link", None)
            .unwrap();
    }
    std::fs::write(
        log_dir.join("output_log_2026-10-21_21-00-00.txt"),
        "2026.10.21 21:23:12 Debug      -  [Behaviour] Unloading scenes\n",
    )
    .unwrap();

    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(db.daemon_errors().unwrap().is_empty());
}

#[test]
fn missing_photo_root_records_a_scan_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let db_path = dir.path().join("library.db");

    let mut daemon = Daemon::open(&db_path, Some(missing), Some(log_dir)).unwrap();
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    let errors = db.daemon_errors().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].operation, DaemonOperation::Scan);
    assert!(errors[0].message.contains("写真フォルダ"));
}

#[test]
fn missing_log_folder_records_a_log_error() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    std::fs::create_dir_all(&photo_dir).unwrap();
    let log_dir = dir.path().join("missing-logs");
    let db_path = dir.path().join("library.db");

    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    let errors = db.daemon_errors().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].operation, DaemonOperation::Log);
}

#[cfg(unix)]
#[test]
fn scan_failure_does_not_mark_unseen_photos_missing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&photo_path);
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    let photo = db.photo_by_path(photo_path.to_str().unwrap()).unwrap().unwrap();
    drop(db);

    let locked = photo_dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::remove_file(&photo_path).unwrap();
    daemon.run_once().unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    let db = Db::open(&db_path).unwrap();
    let photo = db.photo_by_id(photo.id).unwrap().unwrap();
    assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-daemon reads_only_complete_lines_with_offsets
```
Expected: FAIL because `read_new_lines` returns `Vec<String>` without offsets.

- [ ] **Step 3: Implement the daemon error cache and scan reporting**

Add `last_errors` to `Daemon` and load it from `db.daemon_errors()` in `Daemon::open`. Implement `error_signature`, `report_daemon_error`, and `clear_daemon_error` as described in the spec. `report_daemon_error` only writes when the signature changes; `clear_daemon_error` only deletes when the cache had the operation.

Refactor `scan_photos`:
- handle each root independently;
- a missing root pushes `ScanFailure { path, message: "写真フォルダが見つかりません" }`;
- a `scan_and_index` error pushes a failure and continues;
- merge `root_report.failures`;
- run the existing `mark_missing` diff only when `root_report.failures.is_empty()`;
- after dropping the DB guard, report a single `Scan` error with a summary and up to five path/reason lines, or clear it when there are no failures.

`index_dirty_paths` remains `Result`; its caller in `run()` records an error under `Scan` when it fails.

- [ ] **Step 4: Implement log error reporting and offsets**

Change `latest_log_file` to `Result<Option<PathBuf>>`, returning `Ok(None)` for `ErrorKind::NotFound` and propagating other errors.

Change `read_new_lines` to return each line with its end offset and leave a trailing partial line for the next poll.

Add `failed_offset`/`failed_attempts` to `LogTail` and `note_line_failure`:
- if the offset differs from `failed_offset`, reset attempts to 1;
- otherwise increment;
- at `>= 3`, reset the fields and return `true` (skip);
- otherwise return `false`.

Refactor `ingest_logs` into an inner pass plus a report wrapper:
- no configured log dir: clear `Log` and return `Ok`;
- missing log dir: pass `Some("ログフォルダが見つかりません: ...")`;
- `latest_log_file` error or `read_new_lines` error: pass the error;
- process lines in order, calling `set_log_offset` after each successful line;
- on an ingest error, keep the offset before the line, record it, and stop unless `note_line_failure` returns true; when skipping, advance the offset to the line end and continue;
- if any line was skipped or errored, do not clear the error; if at least one line was processed cleanly with no skip, clear the error; no lines means leave the error visible;
- always `Ok` for recoverable data-source errors; DB failures still propagate.

`link_unlinked` becomes `&mut self` and records/clears `Link` around the existing `relink_unlinked` call.

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-daemon
```
Expected: all daemon tests PASS, including the existing end-to-end tests.

- [ ] **Step 6: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: report daemon scan and log failures (#17)"
```

---

## Task 7: Daemon command handling, watcher rebuild, and retry E2E

**Files:**
- Modify: `crates/daemon/src/lib.rs`

**Interfaces:**
- Consumes: Task 2 commands, Task 6 reporting helpers.
- Produces (private):
  - `Daemon::poll_commands`
  - `Daemon::reload_settings`
  - `Daemon::rebuild_watchers`
  - `run_once()` consumes commands before its normal pass.
  - `run()` calls `poll_commands()` every loop.

- [ ] **Step 1: Write the failing tests**

Add:

```rust
#[test]
fn rescan_command_clears_missing_root_error_and_indexes_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
    daemon.run_once().unwrap();

    {
        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            db.daemon_errors().unwrap()[0].operation,
            DaemonOperation::Scan
        );
    }

    std::fs::create_dir_all(&photo_dir).unwrap();
    write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
    {
        let db = Db::open(&db_path).unwrap();
        assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
    }
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(db.daemon_errors().unwrap().is_empty());
    assert_eq!(db.count_photos().unwrap(), 1);
}

#[test]
fn relog_command_clears_a_log_error_and_reads_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    std::fs::create_dir_all(&photo_dir).unwrap();
    let log_dir = dir.path().join("logs");
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir.clone())).unwrap();
    daemon.run_once().unwrap();

    std::fs::create_dir_all(&log_dir).unwrap();
    let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
    std::fs::write(
        &log_path,
        concat!(
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
        ),
    )
    .unwrap();
    {
        let db = Db::open(&db_path).unwrap();
        assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
    }
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(db.daemon_errors().unwrap().is_empty());
    assert!(db.log_offset(log_path.to_str().unwrap()).unwrap().is_some());
}

#[test]
fn command_reloads_settings_and_rebuilds_watchers() {
    let dir = tempfile::tempdir().unwrap();
    let old_root = dir.path().join("old");
    let new_root = dir.path().join("new");
    std::fs::create_dir_all(&old_root).unwrap();
    std::fs::create_dir_all(&new_root).unwrap();
    write_photo(&new_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let db_path = dir.path().join("library.db");

    let mut daemon = Daemon::open(&db_path, Some(old_root), Some(log_dir)).unwrap();
    {
        let db = Db::open(&db_path).unwrap();
        let mut settings = AppSettings::load(&db).unwrap();
        settings.photo_roots = vec![new_root.to_string_lossy().to_string()];
        settings.save(&db).unwrap();
        assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
    }
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.count_photos().unwrap(), 1);
    assert_eq!(daemon.watchers.len(), 1);
    assert!(db.daemon_errors().unwrap().is_empty());
}

#[test]
fn failed_job_can_be_retried_and_completed() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&photo_path);
    let db_path = dir.path().join("library.db");
    {
        let db = Db::open(&db_path).unwrap();
        db.save_profile(&CompressionProfile {
            id: "p".to_string(),
            name: "retry".to_string(),
            format: vrc_photo_core::model::CompressionFormat::Webp,
            suffix: "_retry".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
    }

    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.run_once().unwrap();

    let job = {
        let db = Db::open(&db_path).unwrap();
        let photo = db.photo_by_path(photo_path.to_str().unwrap()).unwrap().unwrap();
        let job = db.enqueue_job(photo.id, "p", "keep").unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        assert!(db.retry_job(&job).unwrap());
        job
    };
    daemon.compress_pending().unwrap();

    let db = Db::open(&db_path).unwrap();
    let job = db.list_jobs(10).unwrap().into_iter().find(|row| row.id == job).unwrap();
    assert_eq!(job.status, QueueStatus::Completed);
    assert!(db.derivatives_for_photo(job.photo_id).unwrap().len() == 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-daemon rescan_command_clears_missing_root_error_and_indexes_the_folder
```
Expected: FAIL because `run_once` does not consume commands.

- [ ] **Step 3: Implement command polling and watcher rebuild**

Add `poll_commands`:
1. take commands (drop the DB lock before running them);
2. if none, return;
3. reload `AppSettings` from the DB;
4. rebuild watchers (drop old watchers, clear the receiver, call `setup_watchers`; record a `Scan` error on failure);
5. execute `Rescan`, `Relog`, `Relink` in order, recording failures under their operations and refreshing status.

Call `poll_commands` at the start of `run_once`, and once per iteration in `run` before periodic work. `run_once` then does its normal pass as before.

In `run`, replace the startup `setup_watchers()?` with a record-and-continue call so a watcher failure does not kill the daemon. Keep `ensure_roots`, `Db` open, and settings save as fatal startup errors.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-daemon
```
Expected: all daemon tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: handle daemon retry commands and rebuild watchers (#17)"
```

---

## Task 8: UI and daemon file logging

**Files:**
- Modify: `crates/ui/Cargo.toml`
- Create: `crates/ui/src/logging.rs`
- Create: `crates/daemon/src/logging.rs`
- Modify: `crates/ui/src/main.rs` (`mod logging;`, init, replace `eprintln!` error paths)
- Modify: `crates/daemon/src/main.rs` (use the shared logging module)
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `vrc_photo_core::logging::{prepare_log_file, UI_LOG_FILE, DAEMON_LOG_FILE}`.
- Produces: `logging::init(process, default_filter) -> Option<PathBuf>` in each binary.

- [ ] **Step 1: Add dependencies and write the modules**

Run:
```bash
cargo add tracing@0.1 --package vrc-photo-ui
cargo add tracing-subscriber@0.3 --features env-filter --package vrc-photo-ui
```
(or add the same workspace dependencies manually to `crates/ui/Cargo.toml`).

Create `crates/ui/src/logging.rs` and `crates/daemon/src/logging.rs` with the same shape:

```rust
pub fn init(process: &str, default_filter: &str) -> Option<std::path::PathBuf> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match vrc_photo_core::logging::prepare_log_file(process)
        .ok()
        .flatten()
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(|file| (path, file))
        }) {
        Some((path, file)) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt as _;
            builder
                .with_writer(std::sync::Mutex::new(file).and(std::io::stderr))
                .init();
            Some(path)
        }
        None => {
            builder.with_writer(std::io::stderr).init();
            None
        }
    }
}
```

Add `mod logging;` to each binary root. In UI `main()`, before `parse_args`, call:
```rust
if logging::init("vrc-photo-manager", "info,vrc_photo_manager=debug").is_none() {
    tracing::warn!("file logging is unavailable; using stderr only");
}
```
In daemon `main()`, replace the inline `tracing_subscriber::fmt()` block with:
```rust
if logging::init("vrc-photo-daemon", "info,vrc_photo_daemon=debug").is_none() {
    tracing::warn!("file logging is unavailable; using stderr only");
}
```

- [ ] **Step 2: Replace swallowed error output in the UI**

Replace `eprintln!` at the library index/page error sites with `tracing::error!`/`tracing::warn!` carrying the same error. Keep the timing probe `eprintln!` (it is an explicit measurement, not an error).

- [ ] **Step 3: Verify the build and logging test**

Run:
```bash
cargo test -p vrc-photo-core prepares_appends_and_rotates_log_file
cargo build -p vrc-photo-ui -p vrc-photo-daemon
```
Expected: test PASS, both binaries build.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ui/Cargo.toml crates/ui/src/logging.rs \
        crates/daemon/src/logging.rs crates/ui/src/main.rs crates/daemon/src/main.rs
git commit -m "feat: write UI and daemon logs to disk (#17)"
```

---

## Task 9: UI state, global alerts, and pure helpers

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `DaemonError`, `CompressionJob`, `MAX_JOB_RETRIES`, `QueueStatus`.
- Produces:
  - `App.daemon_errors`, `App.failed_jobs`, `App.failed_job_count`, `App.failed_job_names`
  - `App::reload_job_state`
  - `fn alert_summaries(failed_jobs: i64, daemon_errors: usize, import_failed: usize) -> Vec<(Screen, String)>`
  - `fn daemon_operation_label(DaemonOperation) -> &'static str`
  - `fn format_timestamp(&str) -> String`
  - `App::global_alerts(&mut self, cx) -> Vec<AnyElement>`

- [ ] **Step 1: Write the failing tests**

Add `alert_summaries, daemon_operation_label, format_timestamp` to the UI test module's `use super::{...}` list and tests:

```rust
#[test]
fn alert_summaries_follow_priority_order() {
    let rows = alert_summaries(2, 1, 3);
    assert_eq!(rows[0].0, Screen::Compression);
    assert!(rows[0].1.contains("2"));
    assert_eq!(rows[1].0, Screen::Monitor);
    assert_eq!(rows[2].0, Screen::Photos);
    assert!(alert_summaries(0, 0, 0).is_empty());
}

#[test]
fn daemon_operation_labels_cover_every_operation() {
    assert_eq!(daemon_operation_label(DaemonOperation::Scan), "写真スキャン");
    assert_eq!(daemon_operation_label(DaemonOperation::Log), "ログ解析");
    assert_eq!(daemon_operation_label(DaemonOperation::Link), "写真の紐付け");
}

#[test]
fn timestamp_formats_rfc3339_and_falls_back_to_raw() {
    let raw = "2026-10-23T03:04:05+00:00";
    let expected = chrono::DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%m/%d %H:%M")
        .to_string();
    assert_eq!(format_timestamp(raw), expected);
    assert_eq!(format_timestamp("not a timestamp"), "not a timestamp");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-ui alert_summaries_follow_priority_order
```
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Add state and helpers**

Add `Debug` to `Screen`. Add `DaemonError` and `DaemonOperation` to the `vrc_photo_core::model` import. Add the four fields and initialize them in `App::new`. Add `reload_job_state()`:

```rust
fn reload_job_state(&mut self) {
    self.jobs = self.db.list_jobs(20).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load recent compression jobs");
        Vec::new()
    });
    self.failed_jobs = self
        .db
        .list_jobs_by_status(QueueStatus::Failed, 100)
        .unwrap_or_default();
    self.failed_job_count = self.db.count_jobs(QueueStatus::Failed).unwrap_or(0);
    self.failed_job_names = self
        .failed_jobs
        .iter()
        .filter_map(|job| {
            self.db
                .photo_by_id(job.photo_id)
                .ok()
                .flatten()
                .map(|photo| (job.photo_id, photo.file_name))
        })
        .collect();
    self.daemon_errors = self.db.daemon_errors().unwrap_or_default();
}
```

Call it from `reload_aux` and `refresh_live`, replacing the direct `self.jobs = ...` assignments.

Implement the three pure helpers with the exact strings above. `format_timestamp` parses RFC3339 and formats `%m/%d %H:%M` in local time, falling back to the original string.

- [ ] **Step 4: Add the global alert bars**

Rewrite `content()` to build the current screen as `AnyElement`, then wrap it:

```rust
v_flex()
    .flex_1()
    .min_h_0()
    .children(self.global_alerts(cx))
    .child(screen)
    .into_any_element()
```

`global_alerts` calls `alert_summaries(self.failed_job_count, self.daemon_errors.len(), 0)` for now (Task 11 adds the import count) and renders each row as a red-tinted horizontal bar with the message and a button that calls `switch_screen(target, cx)`. Use the existing `RED`, `PANEL`, and `BORDER` tokens; keep the row compact (one line, truncate).

- [ ] **Step 5: Run the tests and build**

Run:
```bash
cargo test -p vrc-photo-ui alert_summaries_follow_priority_order
cargo test -p vrc-photo-ui daemon_operation_labels_cover_every_operation
cargo test -p vrc-photo-ui timestamp_formats_rfc3339_and_falls_back_to_raw
cargo build -p vrc-photo-ui
```
Expected: tests PASS and the UI builds.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add UI error state and global alerts (#17)"
```

---

## Task 10: Compression queue failure detail and retry

**Files:**
- Modify: `crates/ui/src/main.rs` (`queue_panel`, retry methods, helpers, tests)

**Interfaces:**
- Consumes: Task 1 job APIs, Task 9 state (`failed_jobs`, `failed_job_count`, `failed_job_names`).
- Produces:
  - `fn job_retry_available(&CompressionJob) -> bool`
  - `fn job_retry_label(&CompressionJob) -> String`
  - `App::retry_job(&mut self, id: String, cx)`
  - `App::retry_all_failed_jobs(&mut self, cx)`

- [ ] **Step 1: Write the failing tests**

Add `CompressionJob, QueueStatus` to the UI test module's `vrc_photo_core::model` import and add `job_retry_available, job_retry_label` to the `use super::{...}` list, then add:

```rust
fn test_job(status: QueueStatus, retry_count: i64) -> CompressionJob {
    CompressionJob {
        id: "job".to_string(),
        photo_id: 1,
        profile_id: "p".to_string(),
        status,
        progress: 0.0,
        error: Some("boom".to_string()),
        output_path: None,
        policy: "keep".to_string(),
        created_at: "now".to_string(),
        started_at: None,
        finished_at: None,
        retry_count,
    }
}

#[test]
fn job_retry_availability_stops_at_the_cap() {
    assert!(job_retry_available(&test_job(QueueStatus::Failed, 0)));
    assert!(job_retry_available(&test_job(QueueStatus::Failed, 2)));
    assert!(!job_retry_available(&test_job(QueueStatus::Failed, 3)));
    assert!(!job_retry_available(&test_job(QueueStatus::Completed, 0)));
    assert!(job_retry_label(&test_job(QueueStatus::Failed, 3)).contains("上限"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
cargo test -p vrc-photo-ui job_retry_availability_stops_at_the_cap
```
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Implement the helpers and retry actions**

`job_retry_available` is `status == Failed && retry_count < MAX_JOB_RETRIES`. `job_retry_label` returns `再試行 {retry_count}/3` while eligible and `再試行の上限（3回）に達しました` at the cap.

Add `retry_job`:
- call `db.retry_job(&id)`;
- on `Ok(true)`: message "再試行をキューに戻しました" and `reload_job_state`;
- on `Ok(false)`: message with the cap explanation;
- on `Err`: message "再試行できません: {error}" and trace;
- `cx.notify()`.

Add `retry_all_failed_jobs`: call `db.retry_failed_jobs()`, report `N 件を再試行に戻しました（上限到達は除外）`, reload, notify.

- [ ] **Step 4: Rework `queue_panel`**

Change the signature to `fn queue_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement`. Render:
- header with recent count (`self.jobs.len()`) and a red `失敗 N` badge;
- failed section from `self.failed_jobs`, each row with:
  - badge "失敗";
  - file name from `self.failed_job_names` or `photo #{id}`;
  - profile name from `self.profiles`;
  - wrapped `job.error`;
  - `job_retry_label`;
  - `Button::new(SharedString::from(format!("retry-job-{}", job.id)))` when available;
- if `failed_job_count > 100`, a note that only the latest 100 are shown;
- "失敗をすべて再試行" button in the header when any failed job is eligible;
- recent non-failed jobs (`self.jobs.iter().filter(|job| job.status != Failed).take(6)`) below;
- empty state unchanged.

Keep the existing 8-row total preview reasonable; the failed section is separate and scrollable with the screen.

- [ ] **Step 5: Run the tests and build**

Run:
```bash
cargo test -p vrc-photo-ui job_retry_availability_stops_at_the_cap
cargo build -p vrc-photo-ui
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: show compression failures and retry them from the queue (#17)"
```

---

## Task 11: Import result panel and failed-file retry

**Files:**
- Modify: `crates/ui/src/main.rs` (`import_dialog`, report view/panel, helpers, tests)

**Interfaces:**
- Consumes: Task 3 importer types, Task 4 scanner types, Task 9 alert wrapper.
- Produces:
  - `struct ImportReportView { mode, dest, imported, skipped, issues, scan_failures, scan_error }`
  - `fn import_issue_counts(&[ImportIssue]) -> (usize, usize)`
  - `fn retryable_sources(&[ImportIssue]) -> Vec<PathBuf>`
  - `App::run_import`, `App::index_import_destination`, `App::retry_import`, `App::import_report_panel`

- [ ] **Step 1: Write the failing tests**

Add `use std::path::PathBuf;` and `use vrc_photo_core::importer::{ImportIssue, ImportIssueKind};` to the UI test module, add `import_issue_counts, retryable_sources` to the `use super::{...}` list, then add:

```rust
#[test]
fn import_issue_counts_separate_failures_and_partial_moves() {
    let issues = vec![
        ImportIssue {
            source: PathBuf::from("/a.png"),
            target: None,
            message: "copy failed".to_string(),
            kind: ImportIssueKind::Failed,
            retryable: true,
        },
        ImportIssue {
            source: PathBuf::from("/b.png"),
            target: Some(PathBuf::from("/dest/b.png")),
            message: "source remains".to_string(),
            kind: ImportIssueKind::PartialMove,
            retryable: false,
        },
    ];
    assert_eq!(import_issue_counts(&issues), (1, 1));
    assert_eq!(retryable_sources(&issues), vec![PathBuf::from("/a.png")]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
cargo test -p vrc-photo-ui import_issue_counts_separate_failures_and_partial_moves
```
Expected: FAIL because the helpers and types do not exist in the UI.

- [ ] **Step 3: Add the report state and helpers**

Add `ImportReportView` and `import_report: Option<ImportReportView>` to `App`. `import_issue_counts` returns `(count of Failed, count of PartialMove)`. `retryable_sources` returns source paths for `Failed` issues with `retryable == true`.

- [ ] **Step 4: Refactor the import flow**

Replace the body of `import_dialog` with:
1. pick files (unchanged);
2. `run_import(files, mode, dest, cx)`.

`run_import` calls `importer::import_files`. On a hard error, set the header message and trace; return. Otherwise build `ImportReportView` from the result, call `index_import_destination`, store it, and set a short header message such as `3 枚を取り込みました（失敗 1 件）`.

`index_import_destination`:
- ensure the destination is in `settings.photo_roots` and save; on save error set `report.scan_error` and trace;
- `db.add_root(dest, "managed")`; on error set `report.scan_error`;
- `scanner::scan_and_index`; on `Ok` extend `report.scan_failures` and generate thumbnails for `new_ids`; on `Err` set `report.scan_error`;
- `linker::relink_unlinked`; on error set/append `report.scan_error`;
- `reload_library(true, cx)`; if it fails, set `report.scan_error` from `self.library_error`.

`retry_import`:
- take `retryable_sources` from the current report; return if empty;
- call the importer with the stored `dest`/`mode`;
- on success, add `result.imported.len()` and `result.skipped`, keep non-retryable/partial issues, and append `result.issues`;
- call `index_import_destination` again;
- replace `import_report`, set the message, notify.

- [ ] **Step 5: Render the panel and extend alerts**

In `photos_screen`, compute the panel before building the layout and insert it after the Photos header:

```rust
let import_panel = self
    .import_report
    .clone()
    .map(|report| self.import_report_panel(&report, cx));
```

The panel shows mode/destination, counts, the scrollable failed list with reasons, partial-move warnings, scan failures, `scan_error`, "失敗分を再試行" (only when retryable sources exist), and "閉じる" (sets `import_report = None`).

Extend `global_alerts` to pass `self.import_report.as_ref().map(|r| import_issue_counts(&r.issues).0).unwrap_or(0)` as the import count.

- [ ] **Step 6: Run the tests and build**

Run:
```bash
cargo test -p vrc-photo-ui import_issue_counts_separate_failures_and_partial_moves
cargo build -p vrc-photo-ui
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: report failed import files and retry them (#17)"
```

---

## Task 12: Monitor error cards, log card, and settings retry request

**Files:**
- Modify: `crates/ui/src/main.rs` (`monitor_screen`, `settings_screen`, helpers)

**Interfaces:**
- Consumes: Task 2 `DaemonError`/`DaemonCommand`, Task 5 log paths, Task 9 helpers.
- Produces:
  - `App::request_daemon_command`
  - `App::daemon_error_card`
  - `App::log_card`
  - `App::open_logs_folder`
  - `fn open_path(&Path) -> Result<(), String>`

- [ ] **Step 1: Implement the command action**

Add `DaemonCommand` to the `vrc_photo_core::model` import, then add:
```rust
fn request_daemon_command(&mut self, command: DaemonCommand, cx: &mut Context<Self>) {
    match self.db.request_daemon_command(command) {
        Ok(true) => self.message = Some("再試行を要求しました。デーモンが処理します。".to_string()),
        Ok(false) => self.message = Some("すでに再試行を要求済みです。".to_string()),
        Err(error) => self.message = Some(format!("再試行を要求できません: {error}")),
    }
    cx.notify();
}
```

- [ ] **Step 2: Add the monitor error and log cards**

In `monitor_screen`, after the stat cards (or before the session card), render a "処理エラー" card when `self.daemon_errors` is non-empty. For each error:
- label from `daemon_operation_label(error.operation)`;
- `error.message` in `RED`;
- `error.detail` in a wrapped monospace block;
- `検出回数 {attempts}` and `format_timestamp(&error.occurred_at)`;
- a "再試行" button calling `request_daemon_command(error.operation.retry_command(), cx)`.

If the daemon is inactive, add a muted note: "デーモン停止中です。起動後に実行されます。"

Add a "ログ" card (also used in Settings) showing:
- `vrc_photo_core::logging::logs_dir()` path, or "ログフォルダを特定できません";
- `UI_LOG_FILE` and `DAEMON_LOG_FILE`;
- a "ログフォルダを開く" button calling `open_logs_folder`.

Implement `open_logs_folder` using `open_path`; on failure set a header message. Implement `open_path` with `explorer` on Windows, `open` on macOS, and `xdg-open` with a `gio open` fallback on other Unix targets.

- [ ] **Step 3: Update Settings path saving**

In the "パスを保存" handler:
- clone the previous settings;
- mutate and save; on save error restore the previous settings, show "設定を保存できません: {error}", trace, and return;
- if roots/log dir/managed dir changed, call `db.request_daemon_command(DaemonCommand::Rescan)`;
- on success show "保存しました。デーモンに再スキャンを要求しました（停止中なら起動後に実行）";
- on command error show "保存しました。デーモン再起動後に反映されます。";
- keep the existing `reload_library` call when roots changed.

Add `self.log_card(cx)` to the Settings screen below "起動と圧縮".

- [ ] **Step 4: Build and run the UI tests**

Run:
```bash
cargo test -p vrc-photo-ui
cargo build -p vrc-photo-ui
```
Expected: all UI tests PASS and the UI builds.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: surface daemon errors and open the log folder (#17)"
```

---

## Task 13: Swallowed-error audit

**Files:**
- Modify: `crates/ui/src/main.rs`
- Modify: `crates/daemon/src/lib.rs`
- Modify: `crates/core/src/importer.rs`
- Modify: `crates/core/src/compressor.rs`
- Modify: `crates/core/src/thumbnail.rs`
- Modify: `crates/core/src/db.rs`

**Interfaces:**
- Consumes: `tracing` in all three crates.
- Produces: no new public API; user-visible failures gain a log record and, where actionable, a UI message.

- [ ] **Step 1: Audit the UI paths**

Replace silent writes/reads with `tracing::warn!`/`debug!` and keep the existing user message where one exists:
- `App::ensure_profiles`, `reload_aux` profile/stats/settings loads (`unwrap_or_default`): warn with the operation.
- `toggle_sidebar`, `toggle_inspector`, `set_reduce_motion` settings saves: warn.
- `edit_profile`, add-profile, and the inline profile editor saves: warn and show a header message on failure.
- `commit_tags`, `save_note`, `set_favorite`, and the trash `mark_missing`: warn; keep the current header message.
- `load_selected_meta` player/tag/derivative loads and `decode_thumbnail`/`load_preview`: debug/warn with photo id and path.
- The import scan/link/thumbnail path is already covered by Task 11; do not duplicate it.

- [ ] **Step 2: Audit the daemon paths**

Replace `let _ =` with explicit handling:
- thumbnail generation (initial and worker): `debug!` for a missing source, `warn!` for an error;
- `mark_missing`: warn with photo id;
- heartbeat `set_setting`: warn;
- log-tail `finish` on shutdown: warn;
- failed-job `update_job`: error with job id;
- thumbnail/watcher channel sends: `debug!` (a closed channel during shutdown is expected).

- [ ] **Step 3: Audit the core paths**

Replace temp-file cleanup ignores with `tracing::debug!` while still returning the original error:
- `importer::copy_atomic` two removals;
- `compressor::write_atomic` removal;
- `thumbnail::ensure_thumbnail_in` removal.

In `Db::configure`, keep the ignored `journal_mode`/`synchronous` pragmas for in-memory databases, but add a comment explaining why they are intentionally ignored.

- [ ] **Step 4: Verify no user-visible silent failure remains**

Run:
```bash
rg -n "let _ =" crates/core/src crates/daemon/src crates/ui/src/main.rs
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: every remaining `let _ =` is a channel send during shutdown or an intentional pragma, and clippy is clean.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs crates/daemon/src/lib.rs crates/core/src/importer.rs \
        crates/core/src/compressor.rs crates/core/src/thumbnail.rs crates/core/src/db.rs
git commit -m "chore: trace swallowed operation errors (#17)"
```

---

## Task 14: Documentation updates

**Files:**
- Modify: `docs/features.md`
- Modify: `docs/ui.md`
- Modify: `docs/data-model.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/testing.md`
- Modify: `docs/architecture.md`
- Modify: `docs/roadmap.md`

**Interfaces:**
- Consumes: the implemented behaviour from Tasks 1–12.
- Produces: user/developer docs that match the code.

- [ ] **Step 1: Update the feature and UI docs**

In `docs/features.md`:
- 3.5: failed jobs show their reason, retry up to 3 times, and "retry all" skips capped jobs.
- 4: import continues after per-file failures; partial moves count as imported plus a warning and are not retried.
- Add a short "エラー表示と再試行" section covering the alert bars, compression queue, import panel, daemon error cards, and log folder.

In `docs/ui.md`:
- library screen: global alert bars and import result panel;
- compression screen: failed section, error text, per-job/all retry, cap;
- monitor screen: processing error cards and log card;
- settings: log card and "save paths requests a rescan".

- [ ] **Step 2: Update the schema and troubleshooting docs**

In `docs/data-model.md`:
- `compression_jobs.retry_count`;
- `daemon_errors` and `daemon_commands` tables;
- `app_settings` note that the heartbeat remains.

In `docs/troubleshooting.md`:
- log folder path (`<data_dir>/logs/`) and the two file names;
- how to use the processing error cards and retry buttons;
- compression retry cap;
- import partial-move warning and failed-file retry;
- daemon-offline queued commands.

In `docs/testing.md`, add the new core/daemon/UI test areas and the manual checks from the spec.

In `docs/architecture.md`, add a short error/reporting flow paragraph under the failure section: daemon writes `daemon_errors`, UI reads it, UI writes `daemon_commands`, daemon consumes on its loop; compression/import errors remain local.

In `docs/roadmap.md`, remove or mark the P1 "エラー表示" row as done.

- [ ] **Step 3: Verify docs links and formatting**

Run:
```bash
rg -n "retry_count|daemon_errors|daemon_commands|ログフォルダ|再試行" docs
```
Expected: each new concept appears in the intended docs.

- [ ] **Step 4: Commit**

```bash
git add docs/features.md docs/ui.md docs/data-model.md docs/troubleshooting.md \
        docs/testing.md docs/architecture.md docs/roadmap.md
git commit -m "docs: document error details, retries, and log files (#17)"
```

---

## Task 15: Full verification, manual QA, and merge

**Files:** none (verification only).

**Interfaces:**
- Consumes: all previous tasks.
- Produces: a green PR that closes #17 and #14.

- [ ] **Step 1: Run the full local gate**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all three commands exit 0.

- [ ] **Step 2: Seed the demo database for visual QA**

Build the UI, create the demo data once, then insert one failed job and one daemon error with `sqlite3` against `/tmp/vrc-photo-manager-demo/library.db`:

```bash
cargo build -p vrc-photo-ui
cargo run -p vrc-photo-ui -- --demo --demo-reset
# In another shell, after the demo DB exists:
sqlite3 /tmp/vrc-photo-manager-demo/library.db \
  "INSERT INTO daemon_errors(operation,message,detail,occurred_at,attempts) \
   VALUES('scan','テスト用のスキャン失敗','/tmp/missing-folder','2026-10-23T10:00:00+00:00',2);"
# Replace the ids with real rows from the demo DB:
# INSERT INTO compression_jobs(id,photo_id,profile_id,status,progress,error,policy,created_at,finished_at)
# VALUES('qa-failed', <photo_id>, '<profile_id>', 'failed', 0, 'テスト用の圧縮失敗', 'keep', '2026-10-23T10:00:00+00:00', '2026-10-23T10:00:00+00:00');
```

- [ ] **Step 3: Visual QA on the current desktop session**

Use the available visual QA skill for the current session (`hyprland-visual-qa`
on Hyprland, `wsl-gpui-testing` under WSL/WSLg) to launch the app and capture:
- Photos with the import alert/panel (after an import attempt);
- Compression with the failed-job section and retry button;
- Monitor with the processing error card and log card.

For the import failure panel, set the managed directory to a read-only folder and import one valid file; if the file dialog cannot produce a failure in the test environment, temporarily seed an `ImportReportView` in `App::new` for the screenshot and remove the hook before committing.

Confirm: no text clipping, the retry button is reachable, the error text wraps, and the log folder path is readable. Capture images under `/tmp/opencode/issue17-*.png`.

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin feat/issue-17-error-retry
gh pr create --base main --head feat/issue-17-error-retry \
  --title "feat: error detail and retry paths (#17)" \
  --body "Implements #17 and the compression retry acceptance criteria of #14.

- show compression failure reasons and retry up to 3 times
- continue imports after per-file failures and retry failed files
- persist daemon scan/log/link errors and retry commands
- write UI/daemon logs under the data directory and open the folder
- document the behaviour and verification

Closes #17
Closes #14"
```

- [ ] **Step 5: Watch CI and merge**

Use the `ci-watch` skill for the branch. On green:
```bash
gh pr merge <number> --merge
cd /home/tommyz7/work/vrc-photo-manager
git checkout main
git pull
git worktree remove ../vrc-photo-manager-issue17
git branch -d feat/issue-17-error-retry
git push origin --delete feat/issue-17-error-retry
```
Do not stay on the feature branch. If `git worktree remove` refuses because of untracked build output, inspect it first; do not force-remove user work.

- [ ] **Step 6: Confirm issues and release state**

```bash
gh issue view 17 --json state --jq .state
gh issue view 14 --json state --jq .state
```
Expected: `CLOSED` for both. Do not create a tag or release.
