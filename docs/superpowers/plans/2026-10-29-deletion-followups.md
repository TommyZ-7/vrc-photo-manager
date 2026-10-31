# Deletion Follow-ups (#155/#156/#157/#158/#160) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Status: completed (2026-10-30).** All tasks were executed and merged in
> PR #169. This document is a post-hoc record; the checkboxes record what was
> done.

**Goal:** Make the deferred-deletion flow deterministic and fail-closed: stable display derivative, no queue starvation, no baseline reset on read failure, unknown reasons blocked, and an accurate warning heading.

**Architecture:** Core SQL/query changes first (#158, #156), then daemon selection logic (#155), then UI snapshot/heading fixes (#157, #160). Each task adds its regression test beside the existing tests in the same file. No schema migrations and no new dependencies.

**Tech Stack:** Rust stable (edition 2024), rusqlite, gpui-kit 0.8.

**Spec:** Approved design in this session (short design, 2026-10-29). No separate spec file; this document records the decisions.

## Global Constraints

- Workspace crates: `vrc-photo-core`, `vrc-photo-daemon`, `vrc-photo-ui`.
- No new dependencies; no DB schema changes.
- Keep deletion order `requested_at, id`.
- UI copy stays Japanese; the exact heading is `{affected} 枚に未完了のジョブがあります（選択 {selected} 枚）。` when `affected < selected`, otherwise `{selected} 枚に未完了のジョブがあります。`.
- Display derivative rule: later profile in `list_profiles()` order wins; when that is tied/unknown, larger derivative id wins; only files that exist on disk are candidates.
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` must pass before push.

## Review Focus

- An unknown `reason` written by a newer version must never delete originals or derivatives; the request must become `blocked` with `unknown_reason`.
- With >100 pending requests stuck behind active jobs, a ready request at position 101+ must still run on the same tick.
- A failed deletion read must not reset `deletion_baseline`, so the first successful poll after recovery must not toast pre-existing blocked requests.
- With a partial multi-selection where only some photos have jobs, the warning heading must show the affected count, not the selection count.
- With compression workers 1–4, the final `display_derivative_id` must point at the same profile and an existing file.

---

### Task 1: Unknown deletion reason is fail-closed (#158)

**Files:**
- Modify: `crates/core/src/model.rs:397-418`
- Modify: `crates/core/src/db.rs:1545-1551` (`request_deletion` guard)
- Modify: `crates/core/src/db.rs:2602-2616` (`row_to_deletion_request`)
- Modify: `crates/ui/src/app/deletion.rs:10-14` (label)
- Modify: `crates/daemon/src/lib.rs:1754-1785` (`run_deletions_once`)
- Modify: `crates/daemon/src/lib.rs:1787-1790` (`execute_deletion` guard)
- Test: `crates/core/src/db.rs` tests module, `crates/daemon/src/lib.rs` tests module

**Interfaces:**
- Consumes: existing `DeletionReason`, `Db::request_deletion`, `Db::block_deletion`, `Db::pending_deletion_requests`.
- Produces: `DeletionReason::Unknown`; `DeletionReason::as_str(Unknown) == "unknown"`; `DeletionReason::parse` still returns `None` for unknown strings.

- [x] **Step 1: Write the failing core test**

Add to `crates/core/src/db.rs` tests (near `manual_request_upgrades_an_active_auto_request`):

```rust
#[test]
fn unknown_deletion_reason_is_preserved_and_cannot_be_requested() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.connection()
        .execute(
            "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
             VALUES (?1, 'future_reason', 'pending', 0, ?2)",
            params![id, now_string()],
        )
        .unwrap();

    let pending = db.pending_deletion_requests(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reason, DeletionReason::Unknown);
    assert_ne!(pending[0].reason, DeletionReason::Manual);
    assert!(
        db.request_deletion(&[id], DeletionReason::Unknown, false)
            .is_err()
    );
}
```

`now_string` is in scope via `use super::*`.

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-core unknown_deletion_reason_is_preserved_and_cannot_be_requested`
Expected: FAIL (no `DeletionReason::Unknown` variant).

- [x] **Step 3: Implement the enum and guard**

In `crates/core/src/model.rs` add the variant and strings:

```rust
pub enum DeletionReason {
    AutoReplace,
    Manual,
    /// A reason written by a newer version. Never safe to execute.
    Unknown,
}

impl DeletionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoReplace => "auto_replace",
            Self::Manual => "manual",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto_replace" => Some(Self::AutoReplace),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}
```

At the start of `Db::request_deletion`:

```rust
if reason == DeletionReason::Unknown {
    return Err(CoreError::Invalid(
        "cannot record an unknown deletion reason".to_string(),
    ));
}
```

In `row_to_deletion_request`:

```rust
reason: DeletionReason::parse(&row.get::<_, String>(3)?)
    .unwrap_or(DeletionReason::Unknown),
```

In `crates/ui/src/app/deletion.rs` add:

```rust
DeletionReason::Unknown => "不明な理由",
```

- [x] **Step 4: Run the core test to verify it passes**

Run: `cargo test -p vrc-photo-core unknown_deletion_reason_is_preserved_and_cannot_be_requested`
Expected: PASS.

- [x] **Step 5: Write the failing daemon regression test**

Add to `crates/daemon/src/lib.rs` tests (near the missing-asset test):

```rust
#[test]
fn unknown_deletion_reason_is_blocked_without_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let original = dir.path().join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&original);
    daemon.run_once().unwrap();

    let db = Db::open(&dir.path().join("library.db")).unwrap();
    let photo = db
        .photo_by_path(original.to_str().unwrap())
        .unwrap()
        .unwrap();
    db.connection()
        .execute(
            "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
             VALUES (?1, 'future_reason', 'pending', 0, ?2)",
            rusqlite::params![photo.id, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();

    daemon.process_deletions().unwrap();

    assert!(original.exists(), "unknown reason must not delete files");
    let request = db.active_deletion_request(photo.id).unwrap().unwrap();
    assert_eq!(request.reason, vrc_photo_core::model::DeletionReason::Unknown);
    assert_eq!(request.status, DeletionStatus::Blocked);
    assert_eq!(request.blocked_reason.as_deref(), Some("unknown_reason"));
}
```

Use the imports already present in the test module; if `rusqlite` is not imported there, call `vrc_photo_core`'s re-export or add `use rusqlite::params;` inside the test module (the daemon crate already depends on rusqlite).

- [x] **Step 6: Run the daemon test to verify it fails**

Run: `cargo test -p vrc-photo-daemon unknown_deletion_reason_is_blocked_without_deleting`
Expected: FAIL (request is treated as Manual and the original is trashed).

- [x] **Step 7: Implement the executor guard**

At the top of the loop in `run_deletions_once`, before `block_deletion_if_jobs_failed`:

```rust
if pending.reason == DeletionReason::Unknown {
    let db = db.lock().unwrap();
    db.block_deletion(pending.id, "unknown_reason", None)?;
    tracing::warn!(
        request = pending.id,
        photo_id = pending.photo_id,
        "unknown deletion reason; request blocked"
    );
    continue;
}
```

At the top of `execute_deletion`, before reading the photo:

```rust
if request.reason == DeletionReason::Unknown {
    anyhow::bail!("unknown deletion reason");
}
```

- [x] **Step 8: Run both tests to verify they pass**

Run: `cargo test -p vrc-photo-core unknown_deletion_reason` and `cargo test -p vrc-photo-daemon unknown_deletion_reason`
Expected: PASS.

- [x] **Step 9: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs crates/ui/src/app/deletion.rs crates/daemon/src/lib.rs
git commit -m "fix(core): fail closed on unknown deletion reasons"
```

---

### Task 2: Pending deletion queries skip active-job requests (#156)

**Files:**
- Modify: `crates/core/src/db.rs:1718-1727` (`pending_deletion_requests`)
- Test: `crates/core/src/db.rs` tests module
- Test: `crates/daemon/src/lib.rs` tests module (starvation regression)

**Interfaces:**
- Consumes: `compression_jobs.status`, `claim_deletion` readiness rule.
- Produces: `Db::pending_deletion_requests(limit)` returns only requests that `claim_deletion` can claim (no `waiting`/`pending`/`processing` job for the photo), still ordered by `requested_at, id`. Requests with `failed` jobs are still returned so `block_deletion_if_jobs_failed` can transition them.

- [x] **Step 1: Write the failing core test**

Add near `claim_deletion_requires_no_active_jobs`:

```rust
#[test]
fn pending_deletion_requests_skip_requests_with_active_jobs() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = |name: &str| vrc_photo_core::model::NewPhoto {
        root_id: root,
        rel_path: name.to_string(),
        abs_path: format!("/photos/{name}"),
        file_name: name.to_string(),
        captured_at_utc: 1_700_000_000,
        captured_at_local: "2026-10-21 21:14:03".to_string(),
        tz_offset_sec: 9 * 3600,
        width: 16,
        height: 10,
        file_size: 10,
        mtime: 1_700_000_000,
        blake3: None,
    };
    let blocked = db.upsert_photo(&photo("blocked.png")).unwrap();
    let ready = db.upsert_photo(&photo("ready.png")).unwrap();
    db.save_profile(&profile("p", "P")).unwrap();

    db.enqueue_job(blocked, "p", "keep").unwrap();
    let blocked_request = db
        .request_deletion(&[blocked], DeletionReason::Manual, false)
        .unwrap()[0];
    let ready_request = db
        .request_deletion(&[ready], DeletionReason::Manual, false)
        .unwrap()[0];

    let pending = db.pending_deletion_requests(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, ready_request);
    assert_ne!(pending[0].id, blocked_request);
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-core pending_deletion_requests_skip_requests_with_active_jobs`
Expected: FAIL (returns 2 rows).

- [x] **Step 3: Update the SQL**

Replace the query in `pending_deletion_requests` with:

```sql
SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force,
       dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at
FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id
WHERE dr.status = 'pending'
  AND NOT EXISTS (
      SELECT 1 FROM compression_jobs j
      WHERE j.photo_id = dr.photo_id
        AND j.status IN ('waiting', 'pending', 'processing')
  )
ORDER BY dr.requested_at, dr.id LIMIT ?1
```

Add a doc comment: requests with failed jobs stay in this list so the executor can mark them blocked.

- [x] **Step 4: Run the core test to verify it passes**

Run: `cargo test -p vrc-photo-core pending_deletion_requests_skip_requests_with_active_jobs`
Expected: PASS.

- [x] **Step 5: Write the failing starvation test**

Add to `crates/daemon/src/lib.rs` tests:

```rust
#[test]
fn ready_deletion_after_more_than_hundred_blocked_requests_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let db = Db::open(&dir.path().join("library.db")).unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let mut request_ids = Vec::new();

    for index in 0..=100i64 {
        let name = format!("p{index:03}.png");
        let photo_id = db
            .upsert_photo(&vrc_photo_core::model::NewPhoto {
                root_id: root,
                rel_path: name.clone(),
                abs_path: format!("/photos/{name}"),
                file_name: name,
                captured_at_utc: 1_700_000_000 + index,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 10,
                mtime: 1_700_000_000 + index,
                blake3: None,
            })
            .unwrap();
        if index < 100 {
            db.enqueue_job(photo_id, "missing-profile", "keep").unwrap();
        }
        let request = db
            .request_deletion(&[photo_id], DeletionReason::Manual, false)
            .unwrap()[0];
        request_ids.push(request);
    }

    daemon.process_deletions().unwrap();

    let ready = db.deletion_request_by_id(request_ids[100]).unwrap().unwrap();
    assert_eq!(ready.status, DeletionStatus::Completed);
    for request_id in &request_ids[..100] {
        let pending = db.deletion_request_by_id(*request_id).unwrap().unwrap();
        assert_eq!(pending.status, DeletionStatus::Pending);
    }
}
```

Note: the manual requests for the 100 blockers are recorded after the job, and the job keeps them unclaimable for the whole tick. Missing files are skipped by `execute_deletion`, which then completes the ready request.

- [x] **Step 6: Run it to verify it fails**

Run: `cargo test -p vrc-photo-daemon ready_deletion_after_more_than_hundred_blocked_requests_runs`
Expected: FAIL (the 101st request never leaves `Pending`).

- [x] **Step 7: Confirm the implementation from Step 3 fixes it**

Run: `cargo test -p vrc-photo-daemon ready_deletion_after_more_than_hundred_blocked_requests_runs`
Expected: PASS.

- [x] **Step 8: Commit**

```bash
git add crates/core/src/db.rs crates/daemon/src/lib.rs
git commit -m "fix(core): skip active-job requests when claiming deletions"
```

---

### Task 3: Deterministic display derivative (#155)

**Files:**
- Modify: `crates/daemon/src/lib.rs` imports and `process_job` (`crates/daemon/src/lib.rs:1932-2009`)
- Modify: `crates/daemon/src/lib.rs` `execute_deletion` auto-replace branch (`crates/daemon/src/lib.rs:1796-1811`)
- Test: `crates/daemon/src/lib.rs` tests module

**Interfaces:**
- Consumes: `db.derivatives_for_photo`, `db.list_profiles` (returns profiles in `sort_order` ascending order), `db.set_replaced`.
- Produces: `fn preferred_display_derivative<'a>(derivatives: &'a [Derivative], profiles: &[CompressionProfile]) -> Option<&'a Derivative>`; returns the last-profile derivative with an existing file, falling back to the largest derivative id for unknown profiles.

- [x] **Step 1: Write the failing helper test**

Add to the daemon tests module:

```rust
#[test]
fn preferred_display_derivative_uses_profile_order_then_id() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.webp");
    let second = dir.path().join("second.webp");
    std::fs::write(&first, b"a").unwrap();
    std::fs::write(&second, b"b").unwrap();
    let derivative = |id: i64, profile: &str, path: &Path| vrc_photo_core::model::Derivative {
        id,
        photo_id: 1,
        path: path.to_string_lossy().to_string(),
        format: "webp".to_string(),
        width: 16,
        height: 10,
        file_size: 1,
        profile_id: profile.to_string(),
        policy: "replace".to_string(),
        created_at: String::new(),
    };
    let derivatives = vec![
        derivative(2, "second", &second),
        derivative(1, "first", &first),
    ];
    let profiles = vec![
        CompressionProfile {
            id: "first".to_string(),
            ..CompressionProfile::default()
        },
        CompressionProfile {
            id: "second".to_string(),
            ..CompressionProfile::default()
        },
    ];

    let picked = preferred_display_derivative(&derivatives, &profiles).unwrap();
    assert_eq!(picked.id, 2);

    // Unknown profiles rank below known ones; among themselves the larger id wins.
    let unknown = vec![
        derivative(5, "gone", &first),
        derivative(7, "gone", &second),
        derivative(9, "gone", &first),
    ];
    let picked = preferred_display_derivative(&unknown, &profiles).unwrap();
    assert_eq!(picked.id, 9);
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-daemon preferred_display_derivative_uses_profile_order_then_id`
Expected: FAIL (`preferred_display_derivative` not defined).

- [x] **Step 3: Implement the helper**

Add near `promote_display_derivative` and import `Derivative` from `vrc_photo_core::model`:

```rust
/// Picks the derivative to show for a photo. Profiles are ordered by
/// `sort_order`, so the later profile in `list_profiles()` wins. Unknown
/// profiles rank below known ones and fall back to the larger derivative id.
/// Only files that still exist are candidates.
fn preferred_display_derivative<'a>(
    derivatives: &'a [Derivative],
    profiles: &[CompressionProfile],
) -> Option<&'a Derivative> {
    derivatives
        .iter()
        .filter(|derivative| Path::new(&derivative.path).is_file())
        .max_by_key(|derivative| {
            (
                profiles
                    .iter()
                    .position(|profile| profile.id == derivative.profile_id),
                derivative.id,
            )
        })
}
```

- [x] **Step 4: Run the helper test to verify it passes**

Run: `cargo test -p vrc-photo-daemon preferred_display_derivative_uses_profile_order_then_id`
Expected: PASS.

- [x] **Step 5: Extend the existing worker-count regression test**

In `multi_profile_replace_is_worker_count_independent`, after the `derivatives` assertions and before the lossless decode check, add:

```rust
let display_id = photo.display_derivative_id.expect("display derivative");
let display = derivatives
    .iter()
    .find(|derivative| derivative.id == display_id)
    .expect("display derivative row");
assert_eq!(display.profile_id, "lossless", "workers={workers}");
assert!(Path::new(&display.path).is_file(), "workers={workers}");
```

- [x] **Step 6: Run it to verify it fails**

Run: `cargo test -p vrc-photo-daemon multi_profile_replace_is_worker_count_independent`
Expected: FAIL intermittently or consistently for workers > 1 (display follows completion order).

- [x] **Step 7: Use the helper in `process_job`**

Drop the now-unused `derivative_id` binding by calling `db.add_derivative(...)?;` directly, then replace the `set_replaced` call in the replace branch with:

```rust
let derivatives = db.derivatives_for_photo(photo.id)?;
let profiles = db.list_profiles()?;
let preferred = preferred_display_derivative(&derivatives, &profiles)
    .context("no displayable derivative after compression")?;
if !db.set_replaced(photo.id, preferred.id)? {
    anyhow::bail!("derivative was not attached to the photo");
}
```

- [x] **Step 8: Use the helper in `execute_deletion`**

In the `AutoReplace && !force` branch, replace the `derivatives.iter().find(...)` selection with:

```rust
let profiles = db.list_profiles()?;
let Some(valid) = preferred_display_derivative(&derivatives, &profiles) else {
    db.block_deletion(request.id, "missing_asset", None)?;
    return Err(anyhow::anyhow!(
        "replacement derivative is missing; refusing to delete the original"
    ));
};
```

Keep the existing `set_replaced` call and condition below it unchanged.

- [x] **Step 9: Run the daemon deletion/compression tests**

Run: `cargo test -p vrc-photo-daemon`
Expected: PASS, including `multi_profile_replace_is_worker_count_independent` for workers 1–4.

- [x] **Step 10: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "fix(daemon): pick the display derivative deterministically"
```

---

### Task 4: Keep the deletion baseline when a read fails (#157)

**Files:**
- Modify: `crates/ui/src/app/live.rs:29-37` (`LiveSnapshot`, `DeletionSnapshot`)
- Modify: `crates/ui/src/app/live.rs:161-176` (apply)
- Modify: `crates/ui/src/app/live.rs:232-253` (`collect_live_snapshot`, `collect_deletions`)
- Test: `crates/ui/src/app/live.rs` tests module

**Interfaces:**
- Consumes: `Db::active_deletion_requests`, `Db::deletion_counts`, `deletion_blocked_transition`.
- Produces: `LiveSnapshot.deletions: Option<DeletionSnapshot>`; `collect_deletions(&Db) -> Option<DeletionSnapshot>`; `fn deletion_update(baseline: Option<(i64, i64)>, snapshot: Option<DeletionSnapshot>) -> Option<(DeletionSnapshot, Option<i64>, (i64, i64))>`.

- [x] **Step 1: Write the failing pure test**

Change the test module import to `use super::{DeletionSnapshot, collect_live_snapshot, deletion_update};`, then add:

```rust
#[test]
fn deletion_update_keeps_the_baseline_when_the_snapshot_is_missing() {
    assert!(deletion_update(Some((1, 2)), None).is_none());

    let snapshot = DeletionSnapshot {
        requests: Vec::new(),
        counts: vrc_photo_core::model::DeletionCounts {
            pending: 1,
            blocked: 2,
            running: 0,
        },
    };
    let (_, blocked, baseline) = deletion_update(Some((0, 0)), Some(snapshot)).unwrap();
    assert_eq!(blocked, Some(2));
    assert_eq!(baseline, (1, 2));

    let snapshot = DeletionSnapshot {
        requests: Vec::new(),
        counts: vrc_photo_core::model::DeletionCounts {
            pending: 1,
            blocked: 2,
            running: 0,
        },
    };
    let (_, blocked, baseline) = deletion_update(Some((1, 2)), Some(snapshot)).unwrap();
    assert_eq!(blocked, None);
    assert_eq!(baseline, (1, 2));
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui deletion_update_keeps_the_baseline_when_the_snapshot_is_missing`
Expected: FAIL (`deletion_update` not defined; `deletions` is not optional).

- [x] **Step 3: Make the snapshot optional**

In `crates/ui/src/app/live.rs`:

```rust
#[derive(Default)]
struct LiveSnapshot {
    // ...
    deletions: Option<DeletionSnapshot>,
    // ...
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DeletionSnapshot {
    requests: Vec<DeletionRequest>,
    counts: DeletionCounts,
}
```

`collect_live_snapshot` uses `deletions: collect_deletions(db),`.

```rust
/// Reads both deletion views. Any failure returns `None` so the caller keeps
/// the previous requests, counts, and notification baseline.
fn collect_deletions(db: &Db) -> Option<DeletionSnapshot> {
    Some(DeletionSnapshot {
        requests: db.active_deletion_requests(20).ok()?,
        counts: db.deletion_counts().ok()?,
    })
}
```

- [x] **Step 4: Add the pure transition helper**

```rust
/// Returns the snapshot to apply, the blocked count to announce, and the new
/// baseline. `None` means the read failed and every previous value must stay.
fn deletion_update(
    baseline: Option<(i64, i64)>,
    snapshot: Option<DeletionSnapshot>,
) -> Option<(DeletionSnapshot, Option<i64>, (i64, i64))> {
    let snapshot = snapshot?;
    let next = (snapshot.counts.pending, snapshot.counts.blocked);
    let blocked = deletion_blocked_transition(baseline, next);
    Some((snapshot, blocked, next))
}
```

- [x] **Step 5: Rewrite the apply block**

Update the existing `snapshot_reports_deletion_counts_and_requests` test to unwrap the optional snapshot:

```rust
let deletions = snapshot.deletions.expect("deletion snapshot");
assert_eq!(deletions.counts.pending, 1);
assert_eq!(deletions.requests.len(), 1);
assert_eq!(deletions.requests[0].photo_id, id);
```

Replace lines 161–176 with:

```rust
if let Some((deletions, blocked, next_baseline)) =
    deletion_update(self.deletion_baseline, snapshot.deletions)
{
    if self.deletion_requests != deletions.requests
        || self.deletion_counts != deletions.counts
    {
        self.deletion_requests = deletions.requests;
        self.deletion_counts = deletions.counts;
        notify = true;
    }
    if let Some(blocked) = blocked {
        self.toast_warning(
            format!("{blocked} 件の削除が保留されています。ダッシュボードを確認してください"),
            cx,
        );
        notify = true;
    }
    self.deletion_baseline = Some(next_baseline);
}
```

- [x] **Step 6: Run the UI tests**

Run: `cargo test -p vrc-photo-ui deletion_update_keeps_the_baseline_when_the_snapshot_is_missing`
Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add crates/ui/src/app/live.rs
git commit -m "fix(ui): keep deletion baseline until a read succeeds"
```

---

### Task 5: Warning heading counts photos with jobs (#160)

**Files:**
- Modify: `crates/core/src/model.rs:495-504` (`DeletionJobSummary`)
- Modify: `crates/core/src/db.rs:1666-1696` (`deletion_job_summary`)
- Modify: `crates/ui/src/app/photos.rs:471-549` (`trash_selected`) and helper area `802-833`
- Test: `crates/core/src/db.rs` (`deletion_job_summary_counts_each_status`)
- Test: `crates/ui/src/app/photos.rs` tests module

**Interfaces:**
- Consumes: `compression_jobs.status`, selection ids.
- Produces: `DeletionJobSummary.affected_photos: i64` (distinct photos with `waiting`/`pending`/`processing`/`failed` jobs); `fn deletion_warning_heading(affected: i64, selected: usize) -> String`.

- [x] **Step 1: Extend the failing core assertion**

In `deletion_job_summary_counts_each_status`, after the existing assertions:

```rust
assert_eq!(summary.affected_photos, 1);

let other = seed_other_photo(&db);
let summary = db.deletion_job_summary(&[id, other]).unwrap();
assert_eq!(summary.affected_photos, 1, "photos without jobs are excluded");
```

Add a small test helper (or inline a second `upsert_photo` with path `/photos/b.png`):

```rust
fn seed_other_photo(db: &Db) -> i64 {
    let root = db.add_root("/photos-b", "reference").unwrap();
    db.upsert_photo(&NewPhoto {
        root_id: root,
        rel_path: "b.png".to_string(),
        abs_path: "/photos-b/b.png".to_string(),
        file_name: "b.png".to_string(),
        captured_at_utc: 1_700_000_001,
        captured_at_local: "2026-10-21 21:14:04".to_string(),
        tz_offset_sec: 9 * 3600,
        width: 16,
        height: 10,
        file_size: 10,
        mtime: 1_700_000_001,
        blake3: None,
    })
    .unwrap()
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p vrc-photo-core deletion_job_summary_counts_each_status`
Expected: FAIL (no `affected_photos` field).

- [x] **Step 3: Add the field and SQL count**

In `model.rs`:

```rust
pub struct DeletionJobSummary {
    pub compression: JobStatusCounts,
    pub send: JobStatusCounts,
    /// Distinct selected photos with at least one unfinished or failed job.
    pub affected_photos: i64,
}
```

In `deletion_job_summary`, inside the chunk loop after the status query:

```rust
affected_photos += self.conn.query_row(
    &format!(
        "SELECT COUNT(DISTINCT photo_id) FROM compression_jobs \
         WHERE photo_id IN ({placeholders}) \
           AND status IN ('waiting', 'pending', 'processing', 'failed')"
    ),
    params_from_iter(chunk.iter().copied()),
    |row| row.get::<_, i64>(0),
)?;
```

Declare `let mut affected_photos = 0;` before the loop and return it in the struct. Update the struct literal test in `photos.rs` to include `affected_photos: 1`.

- [x] **Step 4: Run the core test to verify it passes**

Run: `cargo test -p vrc-photo-core deletion_job_summary_counts_each_status`
Expected: PASS.

- [x] **Step 5: Write the failing UI heading test**

In `crates/ui/src/app/photos.rs` tests, import `deletion_warning_heading` and add:

```rust
#[test]
fn deletion_warning_heading_counts_only_photos_with_jobs() {
    assert_eq!(
        deletion_warning_heading(2, 5),
        "2 枚に未完了のジョブがあります（選択 5 枚）。"
    );
    assert_eq!(
        deletion_warning_heading(3, 3),
        "3 枚に未完了のジョブがあります。"
    );
}
```

- [x] **Step 6: Run it to verify it fails**

Run: `cargo test -p vrc-photo-ui deletion_warning_heading_counts_only_photos_with_jobs`
Expected: FAIL (helper not defined).

- [x] **Step 7: Implement the heading**

Add near `deletion_warning_lines`:

```rust
/// Warning heading for a manual deletion that still has jobs. The first count
/// is the number of selected photos with jobs, not the whole selection.
fn deletion_warning_heading(affected: i64, selected: usize) -> String {
    let selected = selected as i64;
    if affected < selected {
        format!("{affected} 枚に未完了のジョブがあります（選択 {selected} 枚）。")
    } else {
        format!("{selected} 枚に未完了のジョブがあります。")
    }
}
```

In `trash_selected`, compute `let heading = deletion_warning_heading(summary.affected_photos, count);` before building the closure and replace the inline `format!("{count} 枚に未完了のジョブがあります。")` with `heading`.

- [x] **Step 8: Run the UI tests**

Run: `cargo test -p vrc-photo-ui deletion_warning`
Expected: PASS.

- [x] **Step 9: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs crates/ui/src/app/photos.rs
git commit -m "fix(ui): count only photos with jobs in the deletion warning"
```

---

### Task 6: Full verification and PR

**Files:** none (verification only)

- [x] **Step 1: Format, lint, and test**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit 0.

- [x] **Step 2: Commit any formatting fallout**

```bash
git add -u
git commit -m "chore: format deletion follow-up fixes"
```

Skip when `git status` is clean.

- [x] **Step 3: Push and open the PR**

```bash
git push -u origin fix/deletion-followups
gh pr create --title "fix: deletion review follow-ups (#155 #156 #157 #158 #160)" \
  --body "Closes #155, #156, #157, #158, #160

- deterministically pick the display derivative
- skip active-job requests when claiming deletions
- keep the UI deletion baseline until reads succeed
- block unknown deletion reasons without touching files
- count only photos with jobs in the deletion warning"
```

- [x] **Step 4: Watch CI to green and merge**

Use the `ci-watch` skill (background polling). On success:

```bash
gh pr merge <n> --merge
git checkout main && git pull
```

Then delete the branch locally and remotely, and remove the worktree.
