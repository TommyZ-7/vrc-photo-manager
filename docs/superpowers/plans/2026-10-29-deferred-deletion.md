# Deferred Deletion Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every file deletion durable and ordered: queue it in SQLite, drain all jobs that target the photo, then trash the files and update the database, surviving daemon restarts.

**Architecture:** `deletion_requests` is the source of truth. `process_job` stops trashing replaced originals directly; it records an `auto_replace` request instead. A daemon executor thread claims ready requests, trashes files in an idempotent order (derivatives, original, database), and re-evaluates blocked requests when failed jobs are retried. Manual deletion writes a `manual` request and marks the photo `deleting`, so the library hides it immediately while compression jobs finish.

**Tech Stack:** Rust stable (edition 2024), SQLite/rusqlite, gpui-kit 0.8, `trash` 5.

**Spec:** GitHub issue [#143](https://github.com/TommyZ-7/vrc-photo-manager/issues/143) plus the approved decisions below.

## Approved Decisions

- Manual deletion deletes **all** files (derivatives first, then the original) to the OS trash, sets `photos.status='missing'`, and keeps the row and metadata. The current "keep showing the compressed derivative after deleting the original" behavior is removed.
- Auto-replace deletion requests do **not** hide the photo; it stays `replaced`. Only manual deletion uses `status='deleting'`.
- #143 implements the **extension point** for #142's outbox: `deletion_job_summary` exposes a `send` bucket that is zero until #142, and the blocker/cancel logic is centralized so #142 can extend it. No outbox table is created here.
- Force deletion ignores `failed` compression jobs, cancels `waiting`/`pending` jobs for the photo, waits for `processing` jobs, then executes. The confirmation warns that the photo may be the only copy.
- Pending/blocked deletions appear in a dedicated dashboard card, with cancel, retry, and force actions. Blocked transitions produce an in-app toast; the card carries the reason and details.
- Implementation is split into **two code PRs** plus a **docs-only PR** (AGENTS §4): PR1 = core + daemon, PR2 = UI, docs PR = features/data-model/architecture/ADR. PR1 leaves the existing manual-delete UI untouched so `main` stays releasable; manual deletion moves to the queue in PR2 together with its safety UI.

## Global Constraints

- All file deletion goes through the daemon executor. The UI only writes `deletion_requests`, updates local state, bumps `library_revision`, and wakes the daemon. Demo mode is the only exception: it keeps the existing direct trash path because no daemon runs.
- Use `trash::delete` only. Never permanently remove a user file.
- A deletion executes only when every job for the photo is `completed` or `cancelled`. `failed` blocks execution unless `force=1`.
- `waiting`, `pending`, and `processing` jobs are active blockers. `completed` and `cancelled` are terminal success.
- `force=1` ignores `failed` jobs, cancels `waiting`/`pending` compression jobs, and still waits for `processing`.
- Photos with an active deletion request (`pending`, `blocked`, `running`) must not receive new compression jobs.
- A `deleting` photo is hidden from library queries, search, counts, and stats. It stays in the database so cancel can restore it and metadata survives.
- Auto-replace requests never change `photos.status`. `replaced` stays visible while the original is pending removal.
- The executor must be idempotent: every file step skips a missing path, and every request step can be retried after a crash.
- On daemon startup, `running` requests return to `pending` and are re-executed.
- Worker counts 1, 2, 3, and 4 must produce the same final state and job outcomes.
- Run before every push:
  `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- Never mix code and docs in a PR. The docs update is its own PR.

## Review Focus

- Two auto profiles with `replace` and one compression worker: the second job must compress the **original**, not the first job's derivative. Test in Task 7.
- Manual deletion while a compression job is processing: the job must finish without resurrecting the hidden photo, then deletion executes. Test in Task 6.
- Daemon restart after some files were already trashed: the request must finish, not stay `running` forever. Test in Task 6.
- A failed job must leave the original in place and mark the request blocked; retrying the job successfully must resume deletion automatically. Test in Task 7.
- Cancel a blocked manual deletion: the photo must reappear with its previous visible state and no file may have been deleted. Test in Task 4 and Task 10.
- Rescan or watcher upsert while a deletion is pending: the photo must not become visible/present again, and an auto-replaced photo must keep its display derivative. Test in Task 3.
- Force deletion with a `processing` job: deletion must wait, not delete the input under the worker. Test in Task 4.

---

# PR 1: Core + daemon (no UI behavior change)

## Task 1: Preflight worktree

**Files:** none.

- [ ] **Step 1: Confirm `main` is clean and current**

Run:
```bash
git checkout main
git pull
git status --short
```
Expected: no modified files other than the plan/untracked local artifacts you already know about. If `main` is dirty, stop and report.

- [ ] **Step 2: Create the worktree**

Run:
```bash
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-deferred-deletion -b feat/deferred-deletion origin/main
```
Expected: a new worktree at `../vrc-photo-manager-deferred-deletion`.

Copy this plan into the new worktree if it exists only in the original checkout. Do not commit it with either code PR; it travels with the docs PR in Task 14:

```bash
cp /home/tommyz7/work/vrc-photo-manager/docs/superpowers/plans/2026-10-29-deferred-deletion.md \
   ../vrc-photo-manager-deferred-deletion/docs/superpowers/plans/
```

- [ ] **Step 3: Move the session**

Use `tools.opencode.session_move` with `directory` set to the new worktree. Run `git branch --show-current` in the new worktree and confirm `feat/deferred-deletion`.

## Task 2: Deletion schema and model types

**Files:**
- Modify: `crates/core/src/model.rs` (add types near `CompressionJob`)
- Modify: `crates/core/src/db.rs` (`migrate`, schema constants)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Produces:
  - `DeletionReason { AutoReplace, Manual }` with `as_str()` and `parse() -> Option<Self>`
  - `DeletionStatus { Pending, Blocked, Running, Completed, Cancelled }` with `as_str()` / `parse()` / `is_active()`
  - `DeletionRequest { id, photo_id, photo_name, reason, status, force, blocked_reason, error, requested_at, started_at, finished_at }` (`Debug, Clone, PartialEq, Eq`)
  - `DeletionBlockers { active: i64, failed: i64 }` (`Debug, Clone, Copy, Default, PartialEq, Eq`)
  - `JobStatusCounts { waiting, pending, processing, failed }` with `unfinished()` and `total()` (`Debug, Clone, Copy, Default, PartialEq, Eq`)
  - `DeletionJobSummary { compression: JobStatusCounts, send: JobStatusCounts }` with `needs_warning()` (`Debug, Clone, Copy, Default, PartialEq, Eq`)
  - `DeletionCounts { pending: i64, blocked: i64, running: i64 }` (`Debug, Clone, Copy, Default, PartialEq, Eq`)

- [ ] **Step 1: Write the failing migration and round-trip tests**

Add to `crates/core/src/db.rs` tests:

```rust
#[test]
fn migration_v7_adds_deletion_requests() {
    let db = Db::open_in_memory().unwrap();
    let version: i64 = db
        .connection()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
    let table: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name = 'deletion_requests'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table, 1);
    let index: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_deletion_requests_active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
}
```

Add to `crates/core/src/model.rs` tests:

```rust
#[test]
fn deletion_status_and_reason_round_trip() {
    assert_eq!(DeletionStatus::parse("blocked"), DeletionStatus::Blocked);
    assert!(DeletionStatus::Running.is_active());
    assert!(!DeletionStatus::Completed.is_active());
    assert_eq!(
        DeletionReason::parse("auto_replace"),
        Some(DeletionReason::AutoReplace)
    );
    assert_eq!(DeletionReason::Manual.as_str(), "manual");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core migration_v7_adds_deletion_requests
```
Expected: compile errors for the new types, or the migration assertion fails with version 6.

- [ ] **Step 3: Implement the schema**

Add `SCHEMA_V7_DELETION` in `crates/core/src/db.rs`:

```sql
CREATE TABLE IF NOT EXISTS deletion_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    status TEXT NOT NULL,
    force INTEGER NOT NULL DEFAULT 0,
    blocked_reason TEXT,
    error TEXT,
    requested_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_deletion_requests_active
    ON deletion_requests(photo_id)
    WHERE status IN ('pending', 'blocked', 'running');

CREATE INDEX IF NOT EXISTS idx_deletion_requests_status
    ON deletion_requests(status, requested_at);
```

Add `migrate_v7` using the same immediate-transaction/idempotent pattern as `migrate_v6`; call it from `migrate` when `version < 7`, set `user_version` to 7, and insert the `schema_migrations` row.

Update the existing migration tests that assert the final `user_version` from 6 to 7 (`migration_v6_adds_job_output_dir`, `migration_adds_captured_id_index`, `migration_adds_retry_count_and_daemon_tables`, `migration_v5_adds_display_derivative_and_visible_index`, `migration_upgrades_version_3_database_with_existing_jobs`, `migration_v4_recovers_from_interrupted_upgrade`). Do not weaken them to `>= 6`; the exact version is the point.

- [ ] **Step 4: Add the model types and `row_to_deletion_request`**

Use `PhotoStatus`-style enum parsing for the status. `DeletionReason::parse` returns `Option`; the row mapper defaults an unknown reason to `Manual` (the broader deletion scope) and logs nothing. `DeletionRequest.photo_name` is joined from `photos.file_name`; the row mapper reads it as the column after `photo_id`.

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core
```
Expected: PASS, including the updated migration-version assertions.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat(core): add the deletion request schema"
```

## Task 3: Photo `deleting` state and scanner safety

**Files:**
- Modify: `crates/core/src/model.rs` (`PhotoStatus`)
- Modify: `crates/core/src/db.rs` (`upsert_photo`, `find_rebind_candidate`, tests)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Consumes: `DeletionStatus` from Task 2.
- Produces: `PhotoStatus::Deleting`; `upsert_photo` preserves an active deletion's visibility state.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn deleting_photo_is_hidden_from_index_search_and_counts() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.connection()
        .execute("UPDATE photos SET status = 'deleting' WHERE id = ?1", params![id])
        .unwrap();
    assert!(db.photo_index(PhotoSort::Newest).unwrap().is_empty());
    assert_eq!(db.count_photos().unwrap(), 0);
    assert_eq!(db.stats().unwrap().photo_count, 0);
    assert!(db.search_photos(&PhotoFilter::default()).unwrap().is_empty());
}

#[test]
fn upsert_preserves_deleting_status_while_a_request_is_active() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    insert_deletion_request(&db, id, "manual", "pending");
    db.connection()
        .execute("UPDATE photos SET status = 'deleting' WHERE id = ?1", params![id])
        .unwrap();
    let photo = db.photo_by_id(id).unwrap().unwrap();
    db.upsert_photo(&photo_to_new(&photo)).unwrap();
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().status, PhotoStatus::Deleting);
    assert_eq!(db.count_photos().unwrap(), 0);
}

#[test]
fn upsert_preserves_replaced_display_while_auto_deletion_is_active() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let derivative = db.add_derivative(id, "/photos/a_webp.webp", "webp", 16, 10, 8, "p", "replace").unwrap();
    db.set_replaced(id, derivative).unwrap();
    insert_deletion_request(&db, id, "auto_replace", "pending");
    let photo = db.photo_by_id(id).unwrap().unwrap();
    db.upsert_photo(&photo_to_new(&photo)).unwrap();
    let photo = db.photo_by_id(id).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Replaced);
    assert_eq!(photo.display_derivative_id, Some(derivative));
}

#[test]
fn set_replaced_does_not_unhide_a_deleting_photo() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let derivative = db.add_derivative(id, "/photos/a_webp.webp", "webp", 16, 10, 8, "p", "replace").unwrap();
    insert_deletion_request(&db, id, "manual", "pending");
    db.connection()
        .execute("UPDATE photos SET status = 'deleting' WHERE id = ?1", params![id])
        .unwrap();
    assert!(db.set_replaced(id, derivative).unwrap());
    let photo = db.photo_by_id(id).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Deleting);
    assert_eq!(photo.display_derivative_id, Some(derivative));
    assert_eq!(db.count_photos().unwrap(), 0);
}
```

Add these local test helpers:

```rust
fn seed_photo(db: &Db) -> i64 {
    let root = db.add_root("/photos", "reference").unwrap();
    db.upsert_photo(&NewPhoto {
        root_id: root,
        rel_path: "a.png".to_string(),
        abs_path: "/photos/a.png".to_string(),
        file_name: "a.png".to_string(),
        captured_at_utc: 1_700_000_000,
        captured_at_local: "2026-10-21 21:14:03".to_string(),
        tz_offset_sec: 9 * 3600,
        width: 16,
        height: 10,
        file_size: 10,
        mtime: 1_700_000_000,
        blake3: None,
    })
    .unwrap()
}

fn photo_to_new(photo: &Photo) -> NewPhoto {
    NewPhoto {
        root_id: photo.root_id,
        rel_path: photo.rel_path.clone(),
        abs_path: photo.abs_path.clone(),
        file_name: photo.file_name.clone(),
        captured_at_utc: photo.captured_at_utc,
        captured_at_local: photo.captured_at_local.clone(),
        tz_offset_sec: photo.tz_offset_sec,
        width: photo.width,
        height: photo.height,
        file_size: photo.file_size,
        mtime: photo.mtime,
        blake3: photo.blake3.clone(),
    }
}

fn insert_deletion_request(db: &Db, photo_id: i64, reason: &str, status: &str) {
    db.connection()
        .execute(
            "INSERT INTO deletion_requests(photo_id, reason, status, requested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![photo_id, reason, status, "2026-10-29T00:00:00+00:00"],
        )
        .unwrap();
}
```

This keeps Task 3 independent of the Task 4 API.

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core deleting_photo_is_hidden
```
Expected: compile error for `PhotoStatus::Deleting` / `request_deletion`.

- [ ] **Step 3: Implement `PhotoStatus::Deleting`**

Add the variant, `as_str() -> "deleting"`, and `parse("deleting")`. Keep `VISIBLE_STATUS` as `p.status IN ('present', 'replaced')`; no index change is needed.

- [ ] **Step 4: Guard `upsert_photo`**

In the `ON CONFLICT(abs_path) DO UPDATE SET` clause, replace the unconditional `status = 'present'` and `display_derivative_id = NULL` with:

```sql
status = CASE WHEN EXISTS (
    SELECT 1 FROM deletion_requests dr
    WHERE dr.photo_id = photos.id
      AND dr.status IN ('pending', 'blocked', 'running')
) THEN photos.status ELSE 'present' END,
display_derivative_id = CASE WHEN EXISTS (
    SELECT 1 FROM deletion_requests dr
    WHERE dr.photo_id = photos.id
      AND dr.status IN ('pending', 'blocked', 'running')
) THEN photos.display_derivative_id ELSE NULL END,
```

Keep the metadata updates and `updated_at` unchanged. `find_rebind_candidate` already restricts statuses to `present`, `replaced`, `missing`; leave it that way so a deleting row is never rebound.

Also change `set_replaced` so a job that finishes after a manual request was created cannot resurrect the photo:

```sql
UPDATE photos
SET display_derivative_id = ?1,
    status = CASE WHEN status = 'deleting' THEN 'deleting' ELSE 'replaced' END,
    updated_at = ?2
WHERE id = ?3 AND EXISTS (...)
```

It still returns `changed == 1`; the derivative is recorded, but the photo stays hidden. Add `AND status <> 'deleting'` to `restore_present` as the same safeguard.

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat(core): hide photos that are pending deletion"
```

## Task 4: Deletion request lifecycle and enqueue guards

**Files:**
- Modify: `crates/core/src/db.rs` (new deletion-request section near compression jobs)
- Modify: `crates/core/src/db.rs` (`enqueue_job_with_options`, `enqueue_job_for_existing_profile_with_options`, tests)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Consumes: `DeletionReason`, `DeletionStatus`, `DeletionRequest`, `DeletionJobSummary`, `JobStatusCounts`.
- Produces:
  - `Db::request_deletion(&self, photo_ids: &[i64], reason: DeletionReason, force: bool) -> Result<Vec<i64>>`
  - `Db::active_deletion_request(&self, photo_id: i64) -> Result<Option<DeletionRequest>>`
  - `Db::deletion_job_summary(&self, photo_ids: &[i64]) -> Result<DeletionJobSummary>`
  - `Db::active_deletion_requests(&self, limit: i64) -> Result<Vec<DeletionRequest>>`
  - `Db::deletion_counts(&self) -> Result<DeletionCounts>`
  - Enqueue methods return `false` / `CoreError::Invalid` for a photo with an active deletion.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn manual_deletion_request_hides_photo_and_is_idempotent() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let first = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap();
    let second = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap();
    assert_eq!(first, second);
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().status, PhotoStatus::Deleting);
    let request = db.active_deletion_request(id).unwrap().unwrap();
    assert_eq!(request.reason, DeletionReason::Manual);
    assert_eq!(request.status, DeletionStatus::Pending);
}

#[test]
fn auto_replace_request_keeps_the_photo_visible() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.request_deletion(&[id], DeletionReason::AutoReplace, false).unwrap();
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().status, PhotoStatus::Present);
    assert_eq!(db.count_photos().unwrap(), 1);
}

#[test]
fn manual_request_upgrades_an_active_auto_request() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.request_deletion(&[id], DeletionReason::AutoReplace, false).unwrap();
    db.request_deletion(&[id], DeletionReason::Manual, false).unwrap();
    let request = db.active_deletion_request(id).unwrap().unwrap();
    assert_eq!(request.reason, DeletionReason::Manual);
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().status, PhotoStatus::Deleting);
}



#[test]
fn enqueue_rejects_photos_with_active_deletion() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    db.request_deletion(&[id], DeletionReason::Manual, false).unwrap();
    assert!(!db.enqueue_job_for_existing_profile(id, "p", "keep").unwrap());
    assert!(db.enqueue_job(id, "p", "keep").is_err());
    assert_eq!(db.count_active_jobs().unwrap(), 0);
}

#[test]
fn deletion_job_summary_counts_each_status() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    db.enqueue_job(id, "p", "keep").unwrap();
    let waiting = db.enqueue_job(id, "p", "keep").unwrap();
    db.update_job(&waiting, QueueStatus::Waiting, 0.0, None, None).unwrap();
    let failed = db.enqueue_job(id, "p", "keep").unwrap();
    db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None).unwrap();
    let summary = db.deletion_job_summary(&[id]).unwrap();
    assert_eq!(summary.compression.pending, 1);
    assert_eq!(summary.compression.waiting, 1);
    assert_eq!(summary.compression.failed, 1);
    assert_eq!(summary.send.total(), 0);
    assert!(summary.needs_warning());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core
```
Expected: compile errors for missing methods.

- [ ] **Step 3: Implement `request_deletion` and the read methods**

`request_deletion` opens an `Immediate` transaction. For each id:

1. Skip ids that do not exist.
2. Find an active request (`pending`, `blocked`, `running`).
3. If the active request is `running`, do **not** upgrade it: return `CoreError::Invalid("deletion is already running")` and roll back. The executor may already have computed its target set; changing the reason mid-run would leave the photo half-deleted.
4. If a `pending`/`blocked` request exists, update it:
   - `reason = 'manual'` when the incoming reason is manual (manual wins over auto).
   - `force = force OR current.force`.
   - `status = 'pending'` when the incoming force flag is set and the current status is `blocked`.
5. Otherwise insert a new row with `status='pending'`, `requested_at=now_string()`.
6. If `reason == Manual`, set `photos.status='deleting'` (leave `display_derivative_id` intact).
   If `reason == AutoReplace`, do not touch the photo row.
7. Collect the request id.

Return the ids in input order, skipping missing photos. Do **not** bump `library_revision`; callers do that once per user action.

`active_deletion_request` selects `pending|blocked|running` joined to `photos.file_name`. `active_deletion_requests` returns the same columns for those statuses ordered by `requested_at, id`, limited. `deletion_counts` returns counts for `pending`, `blocked`, and `running`.

`deletion_job_summary` must chunk `photo_ids` by 500 like `photos_by_ids`, so a large selection does not exceed SQLite's parameter limit. Count `compression_jobs` per status. Set `send` to `JobStatusCounts::default()` with a comment that #142 fills it.

- [ ] **Step 4: Guard the enqueue paths**

In `enqueue_job_with_options`, change the direct `INSERT` into an `INSERT ... SELECT` that only inserts when:

```sql
EXISTS (SELECT 1 FROM photos WHERE id = ?2 AND status <> 'deleting')
AND NOT EXISTS (
    SELECT 1 FROM deletion_requests
    WHERE photo_id = ?2 AND status IN ('pending', 'blocked', 'running')
)
```

If no row is inserted, return `CoreError::Invalid("photo is pending deletion")`. Keep the UUID id and existing parameters.

Apply the same two predicates to `enqueue_job_for_existing_profile_with_options`, in addition to its existing profile-existence check. It already returns `bool`; return `false` when the photo is blocked.

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): add deletion request lifecycle and enqueue guards"
```

## Task 5: Claim, block, cancel, force, and restart recovery

**Files:**
- Modify: `crates/core/src/db.rs` (deletion-request section, tests)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 4 methods.
- Produces:
  - `Db::deletion_blockers(&self, photo_id: i64) -> Result<DeletionBlockers>`
  - `Db::claim_deletion(&self, request_id: i64) -> Result<bool>`
  - `Db::block_deletion(&self, request_id: i64, reason: &str, error: Option<&str>) -> Result<bool>`
  - `Db::complete_deletion(&self, request_id: i64) -> Result<bool>`
  - `Db::retry_deletion(&self, request_id: i64) -> Result<bool>`
  - `Db::force_deletion(&self, request_id: i64) -> Result<bool>`
  - `Db::cancel_deletion(&self, request_id: i64) -> Result<bool>`
  - `Db::reset_running_deletions(&self) -> Result<usize>`
  - `Db::pending_deletion_requests(&self, limit: i64) -> Result<Vec<DeletionRequest>>` (pending only)
  - `Db::resume_blocked_deletions_for_photo(&self, photo_id: i64) -> Result<usize>`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn claim_deletion_requires_no_active_jobs() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    db.enqueue_job(id, "p", "keep").unwrap();
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    assert!(!db.claim_deletion(request).unwrap());
    let job = db.next_pending_job().unwrap().unwrap();
    db.update_job(&job.id, QueueStatus::Completed, 1.0, None, None).unwrap();
    assert!(db.claim_deletion(request).unwrap());
    assert_eq!(
        db.active_deletion_request(id).unwrap().unwrap().status,
        DeletionStatus::Running
    );
}

#[test]
fn claim_deletion_blocks_failed_jobs_unless_forced() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    let job = db.enqueue_job(id, "p", "keep").unwrap();
    db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None).unwrap();
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    assert!(!db.claim_deletion(request).unwrap());
    assert!(db.force_deletion(request).unwrap());
    assert!(db.claim_deletion(request).unwrap());
}

#[test]
fn force_deletion_cancels_queued_but_not_processing_jobs() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    let processing = db.enqueue_job(id, "p", "keep").unwrap();
    let pending = db.enqueue_job(id, "p", "keep").unwrap();
    let started = db.next_pending_job().unwrap().unwrap();
    assert_eq!(started.id, processing);
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    db.force_deletion(request).unwrap();
    let jobs = db.list_jobs(10).unwrap();
    let by_id = |id: &str| jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(by_id(&pending).status, QueueStatus::Cancelled);
    assert_eq!(by_id(&processing).status, QueueStatus::Processing);
}

#[test]
fn cancel_deletion_restores_visible_status_until_running() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    assert!(db.cancel_deletion(request).unwrap());
    assert_eq!(db.photo_by_id(id).unwrap().unwrap().status, PhotoStatus::Present);
    assert!(db.active_deletion_request(id).unwrap().is_none());
    let second = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    db.claim_deletion(second).unwrap();
    assert!(!db.cancel_deletion(second).unwrap());
}

#[test]
fn reset_running_deletions_makes_requests_retryable() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    db.claim_deletion(request).unwrap();
    assert_eq!(db.reset_running_deletions().unwrap(), 1);
    assert_eq!(
        db.active_deletion_request(id).unwrap().unwrap().status,
        DeletionStatus::Pending
    );
}

#[test]
fn manual_request_is_rejected_while_a_request_is_running() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let request = db.request_deletion(&[id], DeletionReason::AutoReplace, false).unwrap()[0];
    db.claim_deletion(request).unwrap();
    let error = db
        .request_deletion(&[id], DeletionReason::Manual, false)
        .unwrap_err();
    assert!(error.to_string().contains("running"));
    assert_eq!(
        db.active_deletion_request(id).unwrap().unwrap().reason,
        DeletionReason::AutoReplace
    );
}

#[test]
fn cancel_deletion_refuses_after_execution_started() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    db.claim_deletion(request).unwrap();
    db.block_deletion(request, "file_error", Some("permission denied")).unwrap();
    assert!(!db.cancel_deletion(request).unwrap());
    assert_eq!(
        db.active_deletion_request(id).unwrap().unwrap().status,
        DeletionStatus::Blocked
    );
}

#[test]
fn resume_blocked_deletion_waits_for_all_jobs() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    let failed = db.enqueue_job(id, "p", "keep").unwrap();
    db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None).unwrap();
    let request = db.request_deletion(&[id], DeletionReason::Manual, false).unwrap()[0];
    db.block_deletion(request, "job_failed", None).unwrap();
    assert_eq!(db.resume_blocked_deletions_for_photo(id).unwrap(), 0);

    db.retry_job(&failed).unwrap();
    let retried = db.next_pending_job().unwrap().unwrap();
    db.update_job(&retried.id, QueueStatus::Completed, 1.0, None, None).unwrap();
    assert_eq!(db.resume_blocked_deletions_for_photo(id).unwrap(), 1);
    assert_eq!(
        db.active_deletion_request(id).unwrap().unwrap().status,
        DeletionStatus::Pending
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-core
```
Expected: compile errors for missing methods.

- [ ] **Step 3: Implement the lifecycle methods**

`deletion_blockers` returns:

```sql
active = COUNT(*) FROM compression_jobs
         WHERE photo_id = ?1 AND status IN ('waiting','pending','processing')
failed = COUNT(*) FROM compression_jobs
         WHERE photo_id = ?1 AND status = 'failed'
```

Add a comment that #142 extends this with outbox counts.

`claim_deletion` is one atomic `UPDATE ... RETURNING` or `SELECT changes()`:

```sql
UPDATE deletion_requests
SET status = 'running', started_at = ?1, blocked_reason = NULL, error = NULL
WHERE id = ?2 AND status = 'pending'
  AND NOT EXISTS (
      SELECT 1 FROM compression_jobs j
      WHERE j.photo_id = deletion_requests.photo_id
        AND j.status IN ('waiting', 'pending', 'processing')
  )
  AND (
      force = 1 OR NOT EXISTS (
          SELECT 1 FROM compression_jobs j
          WHERE j.photo_id = deletion_requests.photo_id
            AND j.status = 'failed'
      )
  )
```

Return `changed == 1`. `blocked` is deliberately not claimable: a file error must not retry every 500 ms. Dependency blocks return to `pending` through `resume_blocked_deletions_for_photo`, and file errors return via the explicit retry/force actions.

`block_deletion` sets `status='blocked'`, `blocked_reason`, `error`, and returns whether a row changed. Only apply to `pending|blocked|running`; a completed/cancelled request must not be reopened.

`complete_deletion` sets `status='completed'`, `finished_at`, and clears `blocked_reason`/`error`. `retry_deletion` sets `blocked -> pending`, clears `blocked_reason`/`error`, and returns `changed == 1`; it does not touch `force`.

`force_deletion` uses a transaction:
1. Find the request and its photo. Only `pending|blocked` are eligible.
2. `UPDATE deletion_requests SET force=1, status='pending', blocked_reason=NULL, error=NULL WHERE id=?`.
3. `UPDATE compression_jobs SET status='cancelled', progress=0, finished_at=? WHERE photo_id=? AND status IN ('waiting','pending')`.
4. Return `true`.

`cancel_deletion` uses a transaction and only accepts `pending|blocked` **with `started_at IS NULL`**. A blocked request with `started_at` set already trashed at least one file, so cancel must refuse; the user can retry or force instead. Mark the request `cancelled`, `finished_at=now`. If its reason is `Manual` and the photo is still `deleting`, restore:

```sql
UPDATE photos
SET status = CASE WHEN display_derivative_id IS NOT NULL THEN 'replaced' ELSE 'present' END,
    updated_at = ?1
WHERE id = ?2 AND status = 'deleting'
```

`reset_running_deletions` runs `UPDATE deletion_requests SET status='pending', started_at=NULL WHERE status='running'` and returns the changed count. `pending_deletion_requests` returns `pending` only, joined to `photos.file_name`, ordered by `requested_at, id`.

`resume_blocked_deletions_for_photo` updates `blocked` rows with `blocked_reason='job_failed'` back to `pending` only when the photo has no active and no failed jobs:

```sql
UPDATE deletion_requests
SET status = 'pending', blocked_reason = NULL, error = NULL
WHERE photo_id = ?1 AND status = 'blocked' AND blocked_reason = 'job_failed'
  AND NOT EXISTS (
      SELECT 1 FROM compression_jobs j
      WHERE j.photo_id = ?1 AND j.status IN ('waiting', 'pending', 'processing', 'failed')
  )
```

Return the changed count. This is what makes a successful retry resume deletion automatically without polling blocked rows.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-core manual_request_is_rejected
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): claim, block, cancel, and force deletion requests"
```

## Task 6: Daemon deletion executor and replace integration

**Files:**
- Modify: `crates/daemon/src/lib.rs` (imports, `Daemon` struct, `start_workers`, `stop_workers`, `run_once`, `compress_pending`, `process_job`)
- Test: `crates/daemon/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: all `Db` deletion methods from Tasks 2–5; `DeletionReason`.
- Produces:
  - `Daemon::process_deletions(&mut self) -> Result<()>` (test/CLI entry point)
  - `fn run_deletions_once(db: &Arc<Mutex<Db>>) -> Result<usize>`
  - `fn execute_deletion(db: &Arc<Mutex<Db>>, request: &DeletionRequest) -> Result<()>`
  - `deletion_handle: Option<JoinHandle<()>>` on `Daemon`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn replace_job_queues_deletion_instead_of_trashing_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = replace_test_daemon(&dir); // writes one photo, one auto profile, keep_original=false
    daemon.run_once().unwrap();

    // Drive one job directly so the assertion sees the state between job
    // completion and deletion execution.
    let job = {
        let db = daemon.db.lock().unwrap();
        db.next_pending_job().unwrap().unwrap()
    };
    process_job(&daemon.db, &job).unwrap();

    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db.photo_by_path(&original_path(&dir)).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Replaced);
    assert!(original_path(&dir).exists(), "the original survives until the request runs");
    let request = db.active_deletion_request(photo.id).unwrap().unwrap();
    assert_eq!(request.reason, DeletionReason::AutoReplace);
    assert_eq!(request.status, DeletionStatus::Pending);

    daemon.process_deletions().unwrap();
    assert!(!original_path(&dir).exists());
    assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
    assert!(db.active_deletion_request(photo.id).unwrap().is_none());
}

#[test]
fn deletion_waits_for_every_job_of_the_photo() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = replace_test_daemon(&dir);
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db.photo_by_path(&original_path(&dir)).unwrap().unwrap();
    db.save_profile(&CompressionProfile {
        id: "second".to_string(),
        name: "Second".to_string(),
        ..CompressionProfile::default()
    })
    .unwrap();
    db.enqueue_job(photo.id, "second", "keep").unwrap();
    db.request_deletion(&[photo.id], DeletionReason::Manual, false).unwrap();

    daemon.process_deletions().unwrap();
    assert!(original_path(&dir).exists(), "pending job must block deletion");
    daemon.compress_pending().unwrap();
    daemon.process_deletions().unwrap();
    assert!(!original_path(&dir).exists());
    assert_eq!(db.photo_by_id(photo.id).unwrap().unwrap().status, PhotoStatus::Missing);
}

#[test]
fn manual_deletion_removes_derivatives_and_marks_photo_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = replace_test_daemon(&dir);
    daemon.run_once().unwrap();
    daemon.compress_pending().unwrap();
    daemon.process_deletions().unwrap();

    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db.photo_by_path(&original_path(&dir)).unwrap().unwrap();
    let derivative = db.derivatives_for_photo(photo.id).unwrap()[0].path.clone();
    db.request_deletion(&[photo.id], DeletionReason::Manual, false).unwrap();
    daemon.process_deletions().unwrap();

    assert!(!original_path(&dir).exists());
    assert!(!Path::new(&derivative).exists());
    let photo = db.photo_by_id(photo.id).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Missing);
    assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());
    assert_eq!(db.count_photos().unwrap(), 0);
}

#[test]
fn deletion_recovers_after_partial_file_removal() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir); // no auto profiles, so no job blocks the claim
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db.photo_by_path(&original_path(&dir)).unwrap().unwrap();
    let request = db.request_deletion(&[photo.id], DeletionReason::Manual, false).unwrap()[0];
    db.claim_deletion(request).unwrap();
    std::fs::remove_file(original_path(&dir)).unwrap();

    assert_eq!(db.reset_running_deletions().unwrap(), 1);
    daemon.process_deletions().unwrap();
    assert!(db.active_deletion_request(photo.id).unwrap().is_none());
    assert_eq!(db.photo_by_id(photo.id).unwrap().unwrap().status, PhotoStatus::Missing);
}
```

Add helpers in the daemon test module:

```rust
fn original_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png")
}

fn replace_test_daemon(dir: &tempfile::TempDir) -> Daemon {
    // create photos/logs, write_photo(&original_path(dir)),
    // add_auto_profile, set auto_compress_backlog=true and
    // auto_compress_keep_original=false, thumbnail dir, return Daemon
}

fn manual_test_daemon(dir: &tempfile::TempDir) -> Daemon {
    // create photos/logs, write_photo(&original_path(dir)),
    // no auto profile, thumbnail dir, return Daemon
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-daemon
```
Expected: compile error for `process_deletions` / `DeletionReason`, or assertions show the original was trashed immediately.

- [ ] **Step 3: Implement the executor thread**

Add `deletion_handle: Option<JoinHandle<()>>` to `Daemon` and initialize it to `None`.

In `Daemon::open`, after `Db::open(...)` succeeds and before wrapping it in `Arc<Mutex<_>>`, call `db.reset_running_deletions()?`. This is the single startup recovery point for `run`, `run_once`, and `compress_pending`; do not reset from `start_workers`, where a live thread could own a `running` request.

In `start_workers`, spawn the executor once:

```rust
if self.deletion_handle.is_none() {
    let db = self.db.clone();
    let running = self.running.clone();
    let jobs = self.jobs.clone();
    let handle = thread::Builder::new()
        .name("deletion-executor".to_string())
        .spawn(move || deletion_loop(db, running, jobs))
        .expect("cannot spawn deletion worker");
    self.deletion_handle = Some(handle);
}
```

`deletion_loop` mirrors `worker_loop`: call `run_deletions_once` and then `jobs.wait(&mut seen, Duration::from_millis(500))`. Reuse the existing `JobSignal`; `notify()` wakes both compression and deletion loops, and each waiter has its own generation.

In `stop_workers`, take and join `deletion_handle` before/after the compression workers, logging a panic.

- [ ] **Step 4: Implement `run_deletions_once` and `execute_deletion`**

`run_deletions_once`:

1. Read `pending_deletion_requests(100)`.
2. For each request, read `deletion_blockers`.
3. If `active > 0`, continue.
4. If `failed > 0 && !request.force`:
   - Only call `block_deletion(id, "job_failed", None)` when the request is not already blocked with that reason; otherwise the executor would write the same row every 500 ms.
   - Continue.
5. If `claim_deletion(id)` succeeds, call `execute_deletion`; count successful claims.
6. Return the number executed.

`process_job` calls `resume_blocked_deletions_for_photo(photo.id)` best-effort immediately after recording `QueueStatus::Completed`. Both `worker_loop` and `compress_pending` therefore resume a dependency block without duplicating the call. It is a no-op unless the photo has a job-failure block with all jobs now terminal. `force_deletion` and `retry_deletion` already move file-error blocks back to pending.

`execute_deletion`:

1. Take a short DB lock, load the photo and build targets, then release the lock before touching files. If the photo is missing, call `complete_deletion` and return.
2. Build targets:
   - `AutoReplace`: `[photo.abs_path]`.
   - `Manual`: every `derivatives_for_photo` path, then `photo.abs_path`. Deduplicate.
3. For each target, if `Path::is_file()`:
   - `trash::delete(path)`.
   - On error, re-check `Path::exists()`; if it still exists, re-lock, call `block_deletion(id, "file_error", Some(&error.to_string()))`, release, and return `Err(error)`.
4. Re-lock for the database step:
   - `Manual`: in one transaction, `DELETE FROM derivatives WHERE photo_id = ?`, then `UPDATE photos SET status='missing', display_derivative_id=NULL, updated_at=? WHERE id=? AND status='deleting'`.
   - `AutoReplace`: no photo status change. Keep `replaced`.
5. `complete_deletion(id)`, `bump_library_revision()`, release the lock, and `tracing::info!` with the photo id and reason.

Make `run_deletions_once` continue to the next request when one execution fails; the failed request is already blocked.

- [ ] **Step 5: Add `Daemon::process_deletions` and wire the existing entry points**

```rust
pub fn process_deletions(&mut self) -> Result<()> {
    run_deletions_once(&self.db)?;
    Ok(())
}
```

Call `self.process_deletions()?` at the end of `run_once` and after the compression loop in `compress_pending`. This gives tests and `--once` runs the same behavior as the background thread.

- [ ] **Step 6: Change `process_job` input selection**

Replace:

```rust
let source_path = PathBuf::from(photo.effective_path());
```

with:

```rust
let source_path = if Path::new(&photo.abs_path).is_file() {
    PathBuf::from(&photo.abs_path)
} else {
    PathBuf::from(photo.effective_path())
};
```

Only hash the source when it is the original and `photo.blake3.is_none()`; do not hash a fallback derivative into the photo row.

- [ ] **Step 7: Replace the direct trash block**

In `process_job`, after `add_derivative`, re-read the current status under the database lock (a manual request may have arrived during compression), then:

```rust
let current_status = db.photo_by_id(photo.id)?.map(|row| row.status);
if job.policy == "replace" && current_status != Some(PhotoStatus::Deleting) {
    if !db.set_replaced(photo.id, derivative_id)? {
        anyhow::bail!("derivative was not attached to the photo");
    }
    db.request_deletion(&[photo.id], DeletionReason::AutoReplace, false)?;
}
```

Delete the `trash::delete(&source_path)` block and the optimistic restore logic. A `deleting` photo still records the derivative but stays hidden and gets no display assignment; the manual executor removes the derivative later. The job completes normally.

After `db.update_job(&job.id, QueueStatus::Completed, ...)`, add a best-effort resume that must not turn a completed job into a failure:

```rust
if let Err(error) = db.resume_blocked_deletions_for_photo(photo.id) {
    tracing::warn!(photo_id = photo.id, %error, "cannot resume blocked deletion");
}
```

This is the automatic-resume hook for a failed-job block.

- [ ] **Step 8: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-daemon
```
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat(daemon): execute queued deletions after dependent jobs"
```

## Task 7: Multi-profile determinism and failed-job resume

**Files:**
- Modify: `crates/daemon/src/lib.rs` (`mod tests`)
- Test: `crates/daemon/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 6 executor and `process_job`; `vrc_photo_core::image_decode::decode_image` for WebP decoding.
- Produces: regression coverage for the issue's central acceptance criteria.

Use `decode_image` from core instead of `image::open` for the WebP derivatives; core already enables the `webp` feature.

- [ ] **Step 1: Write the failing multi-profile test**

Add a helper that builds two profiles:

```rust
fn multi_profile_test_daemon(dir: &tempfile::TempDir) -> Daemon {
    // create photos/logs, set thumbnail dir, return Daemon::open(...)
    // Does not add an auto profile; the caller adds the two profiles.
}

fn write_gradient_photo(path: &Path) {
    let image = image::RgbaImage::from_fn(64, 64, |x, y| {
        image::Rgba([(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255])
    });
    image.save(path).unwrap();
}

fn add_replace_profiles(db: &Db) {
    db.save_profile(&CompressionProfile {
        id: "lossy".to_string(),
        name: "lossy".to_string(),
        auto_enabled: true,
        format: vrc_photo_core::model::CompressionFormat::Webp,
        quality: 10,
        lossless: false,
        resize_enabled: false,
        output_dir: Some("lossy".to_string()),
        suffix: "_lossy".to_string(),
        ..CompressionProfile::default()
    }).unwrap();
    db.save_profile(&CompressionProfile {
        id: "lossless".to_string(),
        name: "lossless".to_string(),
        auto_enabled: true,
        format: vrc_photo_core::model::CompressionFormat::Webp,
        lossless: true,
        resize_enabled: false,
        output_dir: Some("lossless".to_string()),
        suffix: "_lossless".to_string(),
        ..CompressionProfile::default()
    }).unwrap();
}
```

Write the test:

```rust
#[test]
fn multi_profile_replace_is_worker_count_independent() {
    for workers in [1usize, 4] {
        let dir = tempfile::tempdir().unwrap();
        // Create the daemon and profiles before the scan, so run_once enqueues
        // both auto jobs.
        let mut daemon = multi_profile_test_daemon(&dir);
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            add_replace_profiles(&db);
        }
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
            settings.auto_compress_keep_original = false;
            settings.auto_compress_immediate = true;
        });
        let original = original_path(&dir);
        write_gradient_photo(&original);
        let expected = vrc_photo_core::image_decode::decode_image(&original)
            .unwrap()
            .to_rgba8();
        daemon.run_once().unwrap();
        daemon.settings.compression_workers = workers;
        daemon.start_workers();
        wait_until_idle(&daemon.db, Duration::from_secs(15));
        daemon.stop_workers(); // joins the deletion thread mid-flight
        daemon.process_deletions().unwrap(); // handles anything still pending

        let db = Db::open(&db_path).unwrap();
        let photo = db.photo_by_path(original.to_str().unwrap()).unwrap().unwrap();
        let derivatives = db.derivatives_for_photo(photo.id).unwrap();
        assert_eq!(derivatives.len(), 2, "workers={workers}");
        assert!(derivatives.iter().all(|d| Path::new(&d.path).is_file()));
        assert!(!original.exists(), "workers={workers}");
        assert_eq!(photo.status, PhotoStatus::Replaced);
        assert_eq!(db.count_jobs(QueueStatus::Completed).unwrap(), 2);
        assert_eq!(db.count_jobs(QueueStatus::Failed).unwrap(), 0);
        let completed: i64 = db.connection().query_row(
            "SELECT COUNT(*) FROM deletion_requests WHERE status='completed'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(completed, 1, "workers={workers}");

        let lossless = derivatives.iter().find(|d| d.profile_id == "lossless").unwrap();
        let actual = vrc_photo_core::image_decode::decode_image(Path::new(&lossless.path))
            .unwrap()
            .to_rgba8();
        assert_eq!(actual, expected, "workers={workers}: second job read a derivative");
    }
}
```

`wait_until_idle` polls `count_active_jobs() == 0` and `count_jobs(Waiting) == 0` with a 20 ms sleep and panics after the timeout. `write_gradient_photo` writes a 64×64 RGBA gradient (not a solid color) so the lossy first pass changes pixels visibly.

- [ ] **Step 2: Run the test to verify it fails on the current input selection**

Temporarily keep the old `effective_path()` input selection (or revert just that line) and run:
```bash
cargo test -p vrc-photo-daemon multi_profile_replace_is_worker_count_independent -- --nocapture
```
Expected: the lossless derivative pixels differ from the original, proving the second job read the lossy derivative.

- [ ] **Step 3: Restore the original-first input selection**

If Step 2 required a temporary revert, restore the Task 6 implementation and rerun:
```bash
cargo test -p vrc-photo-daemon multi_profile_replace_is_worker_count_independent -- --nocapture
```
Expected: PASS for workers 1 and 4.

- [ ] **Step 4: Write the failed-job resume test**

```rust
#[test]
fn failed_job_blocks_deletion_until_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = multi_profile_test_daemon(&dir);
    let db_path = dir.path().join("library.db");
    {
        let db = Db::open(&db_path).unwrap();
        add_replace_profiles(&db);
        // Break the lossy profile's output directory with a file.
        std::fs::write(dir.path().join("photos/lossy"), b"not a directory").unwrap();
    }
    save_daemon_settings(&daemon, |settings| {
        settings.auto_compress_backlog = true;
        settings.auto_compress_keep_original = false;
        settings.auto_compress_immediate = true;
    });
    let original = original_path(&dir);
    write_gradient_photo(&original);
    daemon.run_once().unwrap();
    daemon.compress_pending().unwrap();
    daemon.process_deletions().unwrap();

    let db = Db::open(&db_path).unwrap();
    let photo = db.photo_by_path(original.to_str().unwrap()).unwrap().unwrap();
    assert!(original.exists(), "a failed job must keep the original");
    assert_eq!(photo.status, PhotoStatus::Replaced);
    let request = db.active_deletion_request(photo.id).unwrap().unwrap();
    assert_eq!(request.status, DeletionStatus::Blocked);
    assert_eq!(request.blocked_reason.as_deref(), Some("job_failed"));

    // Fix the profile and retry only the failed job.
    std::fs::remove_file(dir.path().join("photos/lossy")).unwrap();
    let failed = db.list_jobs_by_status(QueueStatus::Failed, 10).unwrap();
    assert_eq!(failed.len(), 1);
    db.retry_job(&failed[0].id).unwrap();
    drop(db);
    daemon.compress_pending().unwrap();
    daemon.process_deletions().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(!original.exists());
    assert!(db.active_deletion_request(photo.id).unwrap().is_none());
    assert_eq!(db.count_jobs(QueueStatus::Failed).unwrap(), 0);
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p vrc-photo-daemon
```
Expected: PASS, including both new tests.

- [ ] **Step 6: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "test(daemon): cover multi-profile replace and deletion resume"
```

## Task 8: PR1 verification and merge

**Files:** none.

- [ ] **Step 1: Run the focused suites**

```bash
cargo test -p vrc-photo-core
cargo test -p vrc-photo-daemon
```

- [ ] **Step 2: Run the full local gate**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: all green. If UI tests fail because `PhotoStatus` gained a variant, update exhaustive matches; do not weaken the enum.

- [ ] **Step 3: Push and open PR1**

```bash
git push -u origin feat/deferred-deletion
gh pr create --base main --head feat/deferred-deletion \
  --title "feat(daemon): defer deletions until dependent jobs finish" \
  --body "Implements the core/daemon half of #143. Adds the deletion request queue, photo deleting state, daemon executor, original-first compression input, and worker-count regression tests. Manual deletion UI and the dashboard are in the follow-up PR."
```

- [ ] **Step 4: Monitor CI with the `ci-watch` skill**

Confirm `CI` (and `Release` bundle if triggered) is green. Do not merge red. After merge, switch to `main`, pull, and delete the local/remote branch. Remove the worktree with `git worktree remove` and `git worktree prune`.

---

# PR 2: Manual deletion UI, dashboard, and notifications

## Task 9: PR2 worktree

**Files:** none.

- [ ] **Step 1: Create the worktree from updated `main`**

```bash
git checkout main
git pull
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-deferred-deletion-ui -b feat/deferred-deletion-ui origin/main
```

- [ ] **Step 2: Move the session** with `tools.opencode.session_move` and confirm the branch.

## Task 10: Manual deletion through the queue + warning modal

**Files:**
- Modify: `crates/ui/src/app/photos.rs` (`trash_selected`, `trash_photos`, `trash_photo`)
- Modify: `crates/ui/src/ui/dialogs.rs` or `crates/ui/src/app/photos.rs` (warning content helper)
- Test: `crates/ui/src/app/photos.rs` (`mod tests`) or a pure helper module

**Interfaces:**
- Consumes: `Db::request_deletion`, `Db::deletion_job_summary`, `DeletionJobSummary`, `DeletionReason`.
- Produces:
  - `pub(crate) fn deletion_warning_lines(summary: &DeletionJobSummary) -> Vec<String>`
  - `App::request_trash_photos(&mut self, ids: Vec<i64>, cx: &mut Context<Self>)`
  - Demo mode keeps the existing direct `trash_photos` path.

- [ ] **Step 1: Write the failing helper tests**

```rust
#[test]
fn deletion_warning_lines_list_compression_and_send_counts() {
    let summary = DeletionJobSummary {
        compression: JobStatusCounts {
            waiting: 2,
            pending: 1,
            processing: 1,
            failed: 1,
        },
        send: JobStatusCounts::default(),
    };
    let lines = deletion_warning_lines(&summary);
    assert!(lines.iter().any(|line| line.contains("圧縮") && line.contains("失敗 1")));
    assert!(lines.iter().any(|line| line.contains("送信") && line.contains("なし")));
    assert!(lines.iter().any(|line| line.contains("全ジョブの完了後")));
    assert!(lines.iter().any(|line| line.contains("送信ジョブはキャンセル")));
    assert!(lines.iter().any(|line| line.contains("保留")));
}

#[test]
fn deletion_warning_lines_are_empty_without_jobs() {
    assert!(deletion_warning_lines(&DeletionJobSummary::default()).is_empty());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-ui deletion_warning_lines
```
Expected: compile error for the missing helper.

- [ ] **Step 3: Implement `deletion_warning_lines`**

Format the compression line as:

```text
圧縮: 実行待ち 2 / 処理中 1 / 未実行 1 / 失敗 1
```

List every non-zero category in that order. If `compression.total() == 0`, write `圧縮: なし`. Always render the send line, using `送信: なし` while the `send` bucket is empty (until #142). Append:

```text
・全ジョブの完了後に削除されます。
・送信ジョブはキャンセルされます。
・失敗したジョブがある場合、削除は保留され、通知されます。
```

Return an empty vector when both buckets are empty. This is the condition for skipping the warning modal.

- [ ] **Step 4: Replace the confirm flow in `trash_selected`**

Keep the existing simple alert when `deletion_job_summary` is empty, but change its text to say the deletion is queued:

```text
「<日時>」を削除待ちに追加します。デーモンがOSゴミ箱へ移動します。
```

When the summary is non-empty:

- Build an alert with title `"未完了のジョブがあります"` and a custom `v_flex` description:
  - A heading with the photo count.
  - One row per `deletion_warning_lines` entry.
  - The existing danger confirm button `"削除待ちに追加"` and cancel `"キャンセル"`.
- On confirm, call `request_trash_photos(ids, cx)`.
- If the alert is cancelled, do nothing; no request is written.

The summary must include failed jobs, not only active jobs, because failed jobs block execution. The warning therefore appears for `waiting`, `pending`, `processing`, and `failed`.

- [ ] **Step 5: Implement `request_trash_photos`**

```rust
pub(crate) fn request_trash_photos(&mut self, ids: Vec<i64>, cx: &mut Context<Self>) {
    if self.demo {
        self.trash_photos(ids, cx); // existing direct path for the demo fixture
        return;
    }
    let requested = match self.db.request_deletion(&ids, DeletionReason::Manual, false) {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "cannot request photo deletion");
            self.toast_error(format!("削除を登録できません: {error}"), cx);
            cx.notify();
            return;
        }
    };
    if requested.is_empty() {
        self.toast_warning("削除できる写真がありません", cx);
        cx.notify();
        return;
    }
    // Reuse the local removal block from trash_photos: bump revision once,
    // remove ids from library/search, unpin thumbnails, clear selection,
    // reload_aux, then claim the revision.
    ...
    if self.daemon_active {
        self.toast_success(format!("{} 枚を削除待ちに追加しました", requested.len()), cx);
    } else {
        self.toast_warning(
            format!("{} 枚を削除待ちに追加しました。デーモン起動後に実行されます", requested.len()),
            cx,
        );
    }
    self.send_wake_jobs(cx);
    cx.notify();
}
```

Remove `TrashOutcome::Promoted` handling from the queue path; it is only used by the demo direct path now. Keep `trash_photo`/`trash_photos` for demo, with a doc comment saying demo only.

- [ ] **Step 6: Run the helper tests**

```bash
cargo test -p vrc-photo-ui deletion_warning_lines
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/app/photos.rs
git commit -m "feat(ui): request manual deletion through the daemon queue"
```

## Task 11: Live deletion state and blocked notification

**Files:**
- Modify: `crates/ui/src/app.rs` (App fields)
- Modify: `crates/ui/src/app/live.rs` (`LiveSnapshot`, `collect_live_snapshot`, `apply_live_snapshot`, tests)
- Modify: `crates/ui/src/ui/notifications.rs` (`deletion_blocked_transition`)
- Test: `crates/ui/src/app/live.rs` and `crates/ui/src/ui/notifications.rs`

**Interfaces:**
- Consumes: `Db::active_deletion_requests`, `Db::deletion_counts`, `DeletionRequest`, `DeletionCounts`.
- Produces:
  - `App { deletion_requests: Vec<DeletionRequest>, deletion_counts: DeletionCounts, deletion_baseline: Option<(i64, i64)> }`
  - `pub(crate) fn deletion_blocked_transition(previous: Option<(i64, i64)>, next: (i64, i64)) -> Option<i64>`

The IPC protocol stays unchanged. The existing 3-second live snapshot is the event source; adding a protocol variant would force a version negotiation for a value the UI already reads from SQLite. Do not add `Event::DeletionChanged`.

- [ ] **Step 1: Write the failing tests**

In `live.rs`:

```rust
#[test]
fn snapshot_reports_deletion_counts_and_requests() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.request_deletion(&[id], DeletionReason::Manual, false).unwrap();
    let snapshot = collect_live_snapshot(&db, -1);
    assert_eq!(snapshot.deletions.counts.pending, 1);
    assert_eq!(snapshot.deletions.requests.len(), 1);
    assert_eq!(snapshot.deletions.requests[0].photo_id, id);
}
```

In `notifications.rs`:

```rust
#[test]
fn blocked_deletion_transition_notifies_only_on_increase() {
    assert_eq!(deletion_blocked_transition(None, (2, 1)), None);
    assert_eq!(deletion_blocked_transition(Some((2, 1)), (3, 1)), None);
    assert_eq!(deletion_blocked_transition(Some((3, 1)), (3, 2)), Some(2));
    assert_eq!(deletion_blocked_transition(Some((3, 2)), (3, 1)), None);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-ui deletion
```
Expected: compile errors for missing fields/functions.

- [ ] **Step 3: Implement the snapshot and transition**

Initialize the new `App` fields in `App::new` with `Vec::new()`, `DeletionCounts::default()`, and `None`.

Add a `DeletionSnapshot` to `LiveSnapshot`:

```rust
#[derive(Default)]
struct DeletionSnapshot {
    requests: Vec<DeletionRequest>,
    counts: DeletionCounts,
}
```

`collect_live_snapshot` fills it with `db.active_deletion_requests(20)` and `db.deletion_counts()`, falling back to empty/default on query errors like `collect_jobs` does. `apply_live_snapshot` compares the requests/counts and sets `notify` when they change. After updating `deletion_baseline`, call `deletion_blocked_transition`; when it returns `Some(count)`, call:

```rust
self.toast_warning(
    format!("{count} 件の削除が保留されています。ダッシュボードを確認してください"),
    cx,
);
```

Seed the baseline on the first successful snapshot, exactly like `failure_baseline`, so pre-existing blocked requests do not notify on startup.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p vrc-photo-ui deletion
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/live.rs crates/ui/src/ui/notifications.rs
git commit -m "feat(ui): track pending deletions in the live snapshot"
```

## Task 12: Dashboard deletion card and actions

**Files:**
- Create: `crates/ui/src/app/deletion.rs` (cancel/retry/force actions)
- Modify: `crates/ui/src/app.rs` (add `pub(crate) mod deletion;`)
- Modify: `crates/ui/src/ui/dashboard.rs` (card)
- Test: `crates/ui/src/app/deletion.rs` (pure label/confirmation helpers)

**Interfaces:**
- Consumes: `Db::cancel_deletion`, `Db::retry_deletion`, `Db::force_deletion`, `DeletionRequest`, `DeletionCounts`.
- Produces:
  - `pub(crate) fn deletion_reason_label(reason: DeletionReason) -> &'static str`
  - `pub(crate) fn deletion_status_label(status: DeletionStatus) -> &'static str`
  - `pub(crate) fn deletion_blocked_label(request: &DeletionRequest) -> Option<String>`
  - `App::cancel_deletion`, `App::retry_deletion`, `App::request_force_deletion`, `App::force_deletion`

- [ ] **Step 1: Write the failing label tests**

```rust
#[test]
fn deletion_labels_cover_every_status_and_reason() {
    assert_eq!(deletion_reason_label(DeletionReason::AutoReplace), "自動置換");
    assert_eq!(deletion_reason_label(DeletionReason::Manual), "手動削除");
    assert_eq!(deletion_status_label(DeletionStatus::Pending), "待機中");
    assert_eq!(deletion_status_label(DeletionStatus::Blocked), "保留");
    assert_eq!(deletion_status_label(DeletionStatus::Running), "実行中");
    assert_eq!(deletion_status_label(DeletionStatus::Completed), "完了");
    assert_eq!(deletion_status_label(DeletionStatus::Cancelled), "取消");
}

#[test]
fn blocked_label_explains_job_and_file_failures() {
    let mut request = sample_request(DeletionStatus::Blocked);
    request.blocked_reason = Some("job_failed".to_string());
    assert_eq!(deletion_blocked_label(&request).as_deref(), Some("圧縮ジョブの失敗で保留中"));
    request.blocked_reason = Some("file_error".to_string());
    request.error = Some("permission denied".to_string());
    assert!(deletion_blocked_label(&request).unwrap().contains("permission denied"));
}

fn sample_request(status: DeletionStatus) -> DeletionRequest {
    DeletionRequest {
        id: 1,
        photo_id: 1,
        photo_name: "a.png".to_string(),
        reason: DeletionReason::Manual,
        status,
        force: false,
        blocked_reason: None,
        error: None,
        requested_at: "2026-10-29T00:00:00+00:00".to_string(),
        started_at: None,
        finished_at: None,
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p vrc-photo-ui
```
Expected: compile error for the missing module/functions.

- [ ] **Step 3: Implement the actions**

Add `crates/ui/src/app/deletion.rs` and declare it in `crates/ui/src/app.rs` as `pub(crate) mod deletion;`. Each action:

- `cancel_deletion(id, cx)`: `db.cancel_deletion`; on success bump revision, `reload_library(true)`, `reload_aux`, toast `"削除待ちを取り消しました"`. If it returns `false`, toast `"削除は既に実行中です"` and let the live snapshot refresh the card.
- `retry_deletion(id, cx)`: `db.retry_deletion`; on success `reload_job_state`, `send_wake_jobs`, toast `"削除を再試行します"`.
- `request_force_deletion(id, window, cx)`: open an alert with title `"強制削除"` and description:
  `"失敗したジョブを無視して削除します。この写真が唯一のコピーの場合、写真を失う可能性があります。"`
  On confirm call `force_deletion`.
- `force_deletion(id, cx)`: `db.force_deletion`; bump revision, `reload_job_state`, `reload_library(true)`, `reload_aux`, `send_wake_jobs`, toast `"強制削除を開始しました"`.

Every action should `tracing::warn!` and `toast_error` on DB failure. Do not optimistically remove rows from the dashboard; the live snapshot will update.

- [ ] **Step 4: Build the dashboard card**

In `dashboard_screen`'s right column, after `daemon_card` and before the daemon error card:

```rust
.when(!self.deletion_requests.is_empty(), |this| {
    this.child(self.deletion_card(cx))
})
```

`deletion_card`:

- Title `"削除待ち"`.
- Badge `"待機 {counts.pending}"` and `"保留 {counts.blocked}"`; include running if non-zero.
- Rows for up to 20 requests:
  - `photo_name`
  - `deletion_reason_label`
  - `deletion_status_label`
  - `format_timestamp(&request.requested_at)`
  - `deletion_blocked_label` when blocked.
  - Buttons:
    - `取消` for `pending`, and for `blocked` only when `started_at.is_none()` (a job-failure block). Do not offer cancel after a file error, because some files may already be in the trash.
    - `再試行` for `blocked`.
    - `強制削除` for `blocked` with `blocked_reason == Some("job_failed")`.
- A footer: `"デーモンが依存ジョブの完了後にOSゴミ箱へ移動します"`.
- Use the existing `glass_panel`, `badge`, `meta_row`, and button styles. Do not add a new screen.

- [ ] **Step 5: Run the tests and compile the UI**

```bash
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/deletion.rs crates/ui/src/ui/dashboard.rs
git commit -m "feat(ui): show pending deletions on the dashboard"
```

## Task 13: PR2 verification, visual QA, and merge

**Files:** none.

- [ ] **Step 1: Run the focused UI suite**

```bash
cargo test -p vrc-photo-ui
```

- [ ] **Step 2: Run the full local gate**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: all green.

- [ ] **Step 3: Visual QA with the GNOME Wayland skill**

Use the `gnome-wayland-visual-qa` skill. Seed the demo database at `/tmp/vrc-photo-manager-demo/library.db`:

```bash
rm -rf /tmp/vrc-photo-manager-demo
cargo run -p vrc-photo-ui -- --demo --demo-reset
# after the first launch creates the DB, stop the app
sqlite3 /tmp/vrc-photo-manager-demo/library.db <<'SQL'
INSERT INTO deletion_requests(photo_id, reason, status, force, blocked_reason, requested_at)
VALUES ((SELECT id FROM photos LIMIT 1), 'manual', 'blocked', 0, 'job_failed', datetime('now'));
UPDATE photos SET status = 'deleting'
WHERE id = (SELECT photo_id FROM deletion_requests WHERE status = 'blocked' LIMIT 1);
INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, created_at)
VALUES ('demo-failed', (SELECT photo_id FROM deletion_requests LIMIT 1), 'demo', 'failed', 0, 'keep', datetime('now'));
INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, created_at)
SELECT 'demo-warning', id, 'demo', 'pending', 0, 'keep', datetime('now')
FROM photos WHERE status = 'present' LIMIT 1;
UPDATE app_settings SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
WHERE key = 'library_revision';
SQL
cargo run -p vrc-photo-ui -- --demo --window-size 1440x900
```

Capture the dashboard and verify:

- The `削除待ち` card is visible in the right column without clipping.
- `待機`/`保留` badges and the blocked reason are readable.
- `取消`, `再試行`, and `強制削除` fit at 1280×820 and 1440×900.
- The warning modal appears when trashing a demo photo with an injected incomplete job; its lines wrap instead of overflowing.
- Cancelling the warning writes no request; confirming hides the photo and shows the pending toast.

Clean up with `rm -rf /tmp/vrc-photo-manager-demo` after the check.

- [ ] **Step 4: Open PR2 and monitor CI**

```bash
git push -u origin feat/deferred-deletion-ui
gh pr create --base main --head feat/deferred-deletion-ui \
  --title "feat(ui): queue manual deletions and show pending deletions" \
  --body "Implements the UI half of #143. Manual deletion now writes a deletion request, shows the job warning modal, and exposes pending/blocked deletions on the dashboard with cancel, retry, and force actions."
```

Use the `ci-watch` skill, merge only when green, then switch to `main`, pull, and remove the worktree/branch.

## Task 14: Docs-only PR

**Files:**
- Modify: `docs/features.md` (§7 削除, compression section)
- Modify: `docs/data-model.md` (`deletion_requests`, `photos.status`, invariants)
- Modify: `docs/architecture.md` (daemon executor, drain, restart recovery, UI writes SQLite)
- Create: `docs/adr/0015-deferred-deletion-queue.md`
- Test: docs-only CI fast pass

- [ ] **Step 1: Update `docs/features.md`**

Replace the current §7 with the approved behavior:

- Manual deletion deletes derivatives and the original to the OS trash, sets `missing`, and keeps metadata.
- Deletion is queued; the photo disappears immediately.
- A warning modal appears when jobs exist; it lists counts and explains drain, send cancellation, and failure blocking.
- Failed jobs keep the original; retry resumes deletion automatically.
- Auto-replace deletes the original only after every job for the photo is terminal success.
- Cancel/force behavior and the dashboard surface.

Update the compression section: every job reads the original while it exists; a `replace` job records a deletion request instead of trashing the input.

- [ ] **Step 2: Update `docs/data-model.md`**

Add the `deletion_requests` table and its columns/indexes. Add `deleting` to the photo status list. Rewrite invariant 6 and add:

- Active deletion requests block new jobs.
- `deleting` photos stay in the database and are hidden from visible queries.
- `running` requests reset to `pending` on daemon startup.
- Force ignores failed jobs, cancels queued jobs, and waits for processing.

- [ ] **Step 3: Update `docs/architecture.md`**

Document the executor thread, `JobSignal` wake-up, idempotent file steps, and restart recovery. State that the UI only writes SQLite and wakes the daemon.

- [ ] **Step 4: Write ADR `0015-deferred-deletion-queue.md`**

Include context (order between deletes and async jobs), decision (durable queue, drain rule, manual vs auto scope, force/cancel), and consequences (original retained on failure, outbox extension point, no DB row deletion).

- [ ] **Step 5: Create the docs worktree and open the docs-only PR**

```bash
git checkout main
git pull
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-deferred-deletion-docs -b docs/deferred-deletion origin/main
cp /home/tommyz7/work/vrc-photo-manager/docs/superpowers/plans/2026-10-29-deferred-deletion.md \
   ../vrc-photo-manager-deferred-deletion-docs/docs/superpowers/plans/
cd ../vrc-photo-manager-deferred-deletion-docs
git add docs/features.md docs/data-model.md docs/architecture.md docs/adr/0015-deferred-deletion-queue.md docs/superpowers/plans/2026-10-29-deferred-deletion.md
git commit -m "docs: describe the deferred deletion queue"
git push -u origin docs/deferred-deletion
gh pr create --base main --head docs/deferred-deletion \
  --title "docs: describe the deferred deletion queue" \
  --body "Documents #143: manual/auto deletion scope, deletion_requests, drain/failure behavior, dashboard actions, and the outbox extension point."
```

If the plan file is already tracked in `main` by then, drop the `cp` line and keep the plan out of this commit.

The docs-only fast pass should report green without running the heavy test matrix.

---

## Self-Review Checklist

- [ ] Every issue requirement maps to a task: queue schema (Task 2), deleting state/visibility (Task 3), enqueue blocking (Task 4), executor/restart (Tasks 5–6), replace input/queue (Task 6), manual job policy (Tasks 4 and 10), warning modal (Task 10), failure notification (Task 11), cancel/force (Tasks 5 and 12), multi-profile determinism (Task 7).
- [ ] The outbox is explicitly an extension point; no outbox table or worker is created in this plan.
- [ ] `PhotoStatus::Deleting` is excluded from every visible query without changing `VISIBLE_STATUS`.
- [ ] `set_replaced` and `upsert_photo` cannot resurrect a deleting photo.
- [ ] The UI never calls `trash::delete` outside demo mode.
- [ ] Auto-replace never hides the photo and never deletes a derivative.
- [ ] Manual deletion always deletes derivatives before the original, then updates the row to `missing`.
- [ ] Force cancels only `waiting`/`pending`, never `processing`.
- [ ] Cancel is refused once execution has started, so a partially trashed photo is never restored as if intact.
- [ ] Crash recovery resets `running` to `pending`.
- [ ] Worker count 1 and 4 assertions compare final DB state and derivative pixels.
