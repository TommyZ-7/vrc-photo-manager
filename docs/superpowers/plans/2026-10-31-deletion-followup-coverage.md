# Deletion Follow-up Coverage Implementation Plan (#170/#171/#172/#174/#175)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the deletion follow-ups from PR #169: count non-terminal jobs in the warning, lock derivative survival and persisted reason strings, exercise the live apply path, and mark the completed plan.

**Architecture:** Two PRs. A code PR changes one aggregate SQL predicate and adds three regression tests (core, daemon, UI). A docs PR marks the 2026-10-29 plan complete, records the checkbox rule, and carries this plan plus its short design. No schema changes and no new dependencies.

**Tech Stack:** Rust stable (edition 2024), rusqlite, gpui-kit 0.8.

**Spec:** `docs/superpowers/specs/2026-10-31-deletion-followup-coverage-design.md`

## Global Constraints

- Workspace crates: `vrc-photo-core`, `vrc-photo-daemon`, `vrc-photo-ui`.
- No new dependencies; no DB schema changes.
- UI copy stays Japanese; the existing heading format is unchanged.
- Code and docs land in separate PRs (`AGENTS.md` §4).
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` must pass before push.

## Review Focus

- A job status written by a newer version must still make the photo appear in the warning heading, matching the body count (`QueueStatus::parse` maps it to pending).
- An unknown deletion reason must leave both the original and every derivative file and row untouched.
- `DeletionReason::Unknown` must persist as `"unknown"` while `parse("unknown")` stays `None`, so a future binary cannot read it back as a known reason.
- A failed deletion read must retain requests, counts, and the notification baseline; the next successful read must not toast pre-existing blocked requests.
- The send outbox does not exist yet (#142), so the summary must not claim send coverage.

---

### Task 1: Count non-terminal compression jobs (#170)

**Files:**
- Modify: `crates/core/src/db.rs:1758-1801` (`deletion_job_summary`)
- Test: `crates/core/src/db.rs` tests module (after `deletion_job_summary_counts_each_status`)

**Interfaces:**
- Consumes: `compression_jobs.status`, `QueueStatus::parse`.
- Produces: `DeletionJobSummary.affected_photos` counts distinct selected photos with any status other than `completed`/`cancelled`.

- [x] **Step 1: Write the failing core test**

Add after `deletion_job_summary_counts_each_status`:

```rust
#[test]
fn deletion_job_summary_counts_a_unknown_status_as_affected() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();
    let job = db.enqueue_job(id, "p", "keep").unwrap();
    db.connection()
        .execute(
            "UPDATE compression_jobs SET status = 'future_status' WHERE id = ?1",
            params![job],
        )
        .unwrap();

    let summary = db.deletion_job_summary(&[id]).unwrap();
    assert_eq!(
        summary.compression.pending, 1,
        "unknown statuses parse as pending in the body"
    );
    assert_eq!(
        summary.affected_photos, 1,
        "the heading must not disagree with the body"
    );
}
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-core deletion_job_summary_counts_a_unknown_status_as_affected`
Expected: FAIL (`affected_photos` is 0 because the status list excludes `future_status`).

- [x] **Step 3: Use the non-terminal predicate**

In `deletion_job_summary`, replace the affected-photo query with:

```rust
affected_photos += self.conn.query_row(
    &format!(
        "SELECT COUNT(DISTINCT photo_id) FROM compression_jobs \
         WHERE photo_id IN ({placeholders}) \
           AND status NOT IN ('completed', 'cancelled')"
    ),
    params_from_iter(chunk.iter().copied()),
    |row| row.get::<_, i64>(0),
)?;
```

Update the method's doc comment: unknown statuses count because
`QueueStatus::parse` reports them as pending, and the send bucket still waits
for #142.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vrc-photo-core deletion_job_summary`
Expected: PASS for both summary tests.

- [x] **Step 5: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "fix(core): count non-terminal jobs in deletion warnings"
```

---

### Task 2: Lock derivative survival for unknown reasons (#171)

**Files:**
- Modify: `crates/daemon/src/lib.rs:3775-3808` (`unknown_deletion_reason_is_blocked_without_deleting`)

**Interfaces:**
- Consumes: `Db::add_derivative`, `Db::derivatives_for_photo`, `Daemon::process_deletions`.
- Produces: a regression test that fails if an unknown-reason guard ever moves behind derivative removal.

- [x] **Step 1: Prepare a derivative and extend the assertions**

In `unknown_deletion_reason_is_blocked_without_deleting`, after loading `photo`,
create a derivative file and row:

```rust
let derivative = dir.path().join("photos/kept.webp");
std::fs::write(&derivative, b"kept derivative").unwrap();
db.add_derivative(
    photo.id,
    derivative.to_str().unwrap(),
    "webp",
    16,
    10,
    std::fs::metadata(&derivative).unwrap().len(),
    "p",
    "keep",
)
.unwrap();
```

After `daemon.process_deletions().unwrap()`, keep the existing assertions and
add:

```rust
assert!(
    derivative.exists(),
    "unknown reason must not delete derivatives"
);
assert_eq!(
    db.derivatives_for_photo(photo.id).unwrap().len(),
    1,
    "unknown reason must not drop the derivative row"
);
```

- [x] **Step 2: Run the test**

Run: `cargo test -p vrc-photo-daemon unknown_deletion_reason_is_blocked_without_deleting`
Expected: PASS. The guard already blocks before touching files; the new
assertions lock the derivative behavior against future guard movement.

- [x] **Step 3: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "chore(daemon): keep derivatives when the deletion reason is unknown"
```

---

### Task 3: Pin the persisted Unknown reason string (#172)

**Files:**
- Modify: `crates/core/src/model.rs:703-713` (`deletion_status_and_reason_round_trip`)

**Interfaces:**
- Consumes: `DeletionReason::as_str`, `DeletionReason::parse`.
- Produces: pinned values `"unknown"` (write side) and `None` (read side).

- [x] **Step 1: Extend the round-trip test**

Add to `deletion_status_and_reason_round_trip`:

```rust
assert_eq!(DeletionReason::Unknown.as_str(), "unknown");
assert_eq!(DeletionReason::parse("unknown"), None);
assert_eq!(DeletionReason::parse("future_reason"), None);
assert_eq!(
    DeletionReason::parse("manual"),
    Some(DeletionReason::Manual)
);
```

- [x] **Step 2: Run the test**

Run: `cargo test -p vrc-photo-core deletion_status_and_reason_round_trip`
Expected: PASS. This is a coverage lock for existing behavior.

- [x] **Step 3: Commit**

```bash
git add crates/core/src/model.rs
git commit -m "chore(core): pin the persisted unknown deletion reason"
```

---

### Task 4: Exercise the live apply path (#174)

**Files:**
- Modify: `crates/ui/src/app/live.rs:161-181` (`apply_live_snapshot` deletion branch)
- Modify: `crates/ui/src/app/live.rs:260-270` (helpers)
- Test: `crates/ui/src/app/live.rs` tests module

**Interfaces:**
- Consumes: `deletion_update`, `DeletionSnapshot`.
- Produces: `fn apply_deletion_snapshot(requests: &mut Vec<DeletionRequest>, counts: &mut DeletionCounts, baseline: &mut Option<(i64, i64)>, snapshot: Option<DeletionSnapshot>) -> Option<DeletionApply>`; `DeletionApply { changed: bool, blocked: Option<i64> }`.

- [x] **Step 1: Write the failing apply-path test**

Change the test import to:

```rust
use super::{
    DeletionSnapshot, apply_deletion_snapshot, collect_live_snapshot, deletion_update,
};
use vrc_photo_core::model::{
    DeletionCounts, DeletionReason, DeletionRequest, DeletionStatus, NewPhoto,
};
```

Add:

```rust
#[test]
fn failed_deletion_snapshot_keeps_all_state_and_avoids_a_false_toast() {
    let mut requests = vec![DeletionRequest {
        id: 1,
        photo_id: 7,
        photo_name: "a.png".to_string(),
        reason: DeletionReason::Manual,
        status: DeletionStatus::Blocked,
        force: false,
        blocked_reason: Some("unknown_reason".to_string()),
        error: None,
        requested_at: String::new(),
        started_at: None,
        finished_at: None,
    }];
    let mut counts = DeletionCounts {
        pending: 1,
        blocked: 1,
        running: 0,
    };
    let mut baseline = Some((1, 1));

    let update = apply_deletion_snapshot(&mut requests, &mut counts, &mut baseline, None);
    assert!(update.is_none(), "a failed read must not apply");
    assert_eq!(requests.len(), 1, "requests survive a failed read");
    assert_eq!(
        counts,
        DeletionCounts {
            pending: 1,
            blocked: 1,
            running: 0,
        }
    );
    assert_eq!(baseline, Some((1, 1)), "baseline survives a failed read");

    let snapshot = DeletionSnapshot {
        requests: requests.clone(),
        counts,
    };
    let update = apply_deletion_snapshot(&mut requests, &mut counts, &mut baseline, Some(snapshot))
        .expect("a successful snapshot applies");
    assert!(!update.changed);
    assert!(
        update.blocked.is_none(),
        "existing blocked requests must not toast again"
    );
}
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-ui failed_deletion_snapshot_keeps_all_state_and_avoids_a_false_toast`
Expected: FAIL to compile (`apply_deletion_snapshot` is not defined).

- [x] **Step 3: Add the apply helper**

Below `deletion_update`:

```rust
/// Outcome of applying one deletion snapshot to the retained UI state.
struct DeletionApply {
    changed: bool,
    blocked: Option<i64>,
}

/// Applies the deletion part of a live snapshot. `None` means the read failed:
/// requests, counts, and the baseline stay exactly as they were, and no toast
/// is produced. A missing snapshot must never reset the baseline, otherwise the
/// next successful read would announce pre-existing blocked requests.
fn apply_deletion_snapshot(
    requests: &mut Vec<DeletionRequest>,
    counts: &mut DeletionCounts,
    baseline: &mut Option<(i64, i64)>,
    snapshot: Option<DeletionSnapshot>,
) -> Option<DeletionApply> {
    let (deletions, blocked, next_baseline) = deletion_update(*baseline, snapshot)?;
    let changed = *requests != deletions.requests || *counts != deletions.counts;
    *requests = deletions.requests;
    *counts = deletions.counts;
    *baseline = Some(next_baseline);
    Some(DeletionApply { changed, blocked })
}
```

- [x] **Step 4: Route `apply_live_snapshot` through the helper**

Replace the deletion `if let` block with:

```rust
if let Some(update) = apply_deletion_snapshot(
    &mut self.deletion_requests,
    &mut self.deletion_counts,
    &mut self.deletion_baseline,
    snapshot.deletions,
) {
    if update.changed {
        notify = true;
    }
    if let Some(blocked) = update.blocked {
        self.toast_warning(
            format!("{blocked} 件の削除が保留されています。ダッシュボードを確認してください"),
            cx,
        );
        notify = true;
    }
}
```

- [x] **Step 5: Run the UI tests**

Run: `cargo test -p vrc-photo-ui failed_deletion_snapshot_keeps_all_state_and_avoids_a_false_toast deletion_update_keeps_the_baseline`
Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/ui/src/app/live.rs
git commit -m "chore(ui): cover the live deletion apply path"
```

---

### Task 5: Mark the completed plan and record the rule (#175, docs PR)

**Files:**
- Modify: `docs/superpowers/plans/2026-10-29-deletion-followups.md`
- Modify: `AGENTS.md` §4
- Add: `docs/superpowers/specs/2026-10-31-deletion-followup-coverage-design.md`
- Add: `docs/superpowers/plans/2026-10-31-deletion-followup-coverage.md` (this plan)

**Interfaces:**
- Consumes: the merged work from PR #169.
- Produces: a completed plan record and a standing checkbox rule.

- [x] **Step 1: Mark the 2026-10-29 plan complete**

Change every `- [x]` to `- [x]` in
`docs/superpowers/plans/2026-10-29-deletion-followups.md`. Add below the title:

```markdown
> **Status: completed (2026-10-30).** All tasks were executed and merged in
> PR #169. This document is a post-hoc record; the checkboxes record what was
> done.
```

- [x] **Step 2: Add the checkbox rule to AGENTS.md §4**

Append to the commit-conventions section:

```markdown
- 計画ファイル (`docs/superpowers/plans/`) をコミットする場合、実行完了時に
  チェックボックスを `- [x]` に更新する。
```

- [x] **Step 3: Confirm no unchecked steps remain**

Run: `grep -n -- '- \[ \]' docs/superpowers/plans/2026-10-29-deletion-followups.md`
Expected: no output.

- [x] **Step 4: Commit**

```bash
git add AGENTS.md docs/superpowers/plans/2026-10-29-deletion-followups.md \
  docs/superpowers/plans/2026-10-31-deletion-followup-coverage.md \
  docs/superpowers/specs/2026-10-31-deletion-followup-coverage-design.md
git commit -m "docs: record the completed deletion follow-up plan"
```

---

### Task 6: Verify, publish, and split the send follow-up

**Files:** none (verification and release work)

- [x] **Step 1: Run the full local gate on the code branch**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all exit 0.

- [x] **Step 2: Create the send-side follow-up issue**

Created as #216.

```bash
gh issue create \
  --title "test(core): send outbox の削除警告集計を検証する" \
  --body "## 背景
#170 で \`affected_photos\` を非終端ジョブから算出するよう修正したが、#142 の送信 outbox は未実装のため \`send\` 側は未検証。

## 期待する状態
- #142 の outbox 追加後、send ジョブを持つ写真が \`affected_photos\` に含まれることをテストする。
- compression と send の両方にジョブがある写真を重複カウントしない。

## 関連
- #142
- #170" \
  --label "triage,area: core,area: daemon"
```

- [ ] **Step 3: Push and open the code PR**

```bash
git push -u origin fix/deletion-followup-coverage
gh pr create --title "fix: deletion follow-up coverage (#170 #171 #172 #174)" \
  --label bug \
  --body "Closes #170, #171, #172, #174

- count non-terminal compression jobs in the deletion warning (#170)
- assert derivatives survive an unknown deletion reason (#171)
- pin the persisted unknown reason string (#172)
- exercise the live deletion apply path (#174)

The send-side count lands with #142; the follow-up issue tracks that test."
```

- [ ] **Step 4: Push and open the docs PR**

```bash
git push -u origin docs/deletion-followups-complete
gh pr create --title "docs: complete the deletion follow-up plan (#175)" \
  --body "Closes #175

- mark every step of the 2026-10-29 deletion follow-up plan complete
- record the plan checkbox rule in AGENTS.md
- add the short design and implementation plan for #170/#171/#172/#174"
```

- [ ] **Step 5: Watch CI and merge**

Use the `ci-watch` skill for both PRs. On success merge each, then switch to
`main`, pull, and delete the work branches and worktrees.
