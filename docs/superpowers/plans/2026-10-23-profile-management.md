# Compression Profile Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist a user-visible compression-profile order, add duplication after the source, and add a guarded delete flow with a modal confirmation.

**Architecture:** Core owns the transactional order/duplicate/delete rules so they are testable against SQLite. The UI only swaps adjacent ids, calls the core methods, and renders the result. Automatic compression already reads `list_profiles()` in order; a rowid tie-break makes the queue consume jobs in that same order.

**Tech Stack:** Rust 2024, `gpui-kit = "=0.8.0"` / `gpui-pre 0.3.9` / `gpui-component 0.8.0` (`AlertDialog`), SQLite via `rusqlite 0.40`.

**Spec:** `docs/superpowers/specs/2026-10-23-profile-management-design.md`

## Global Constraints

- Do not change the database schema, `user_version`, or add a migration.
- Do not add Cargo dependencies or bump `gpui-kit`.
- Keep `crates/ui/src/main.rs` as the single UI file.
- Do not change the note/tag behavior merged in #10.
- Profile insertion order is `MAX(sort_order) + 1`; editing a profile must never move it.
- Duplicate copies are inserted directly after the source, get a unique name, and set `auto_enabled = false`.
- Deletion is blocked while a `pending` or `processing` job references the profile. Completed/failed/cancelled jobs and derivatives remain.
- At least one profile must remain; deleting the last one is refused.
- Automatic enqueue uses an existence-guarded insert so a profile deleted by another process cannot gain a new pending job.
- Use `gpui-component`'s `AlertDialog` for confirmation; do not add a custom modal layer.
- Work in a git worktree on `feat/issue-11-profile-management`; never push to `main`.
- Documentation is a separate docs-only PR per `AGENTS.md` §4.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Do not create tags, run the Release workflow manually, or publish a release.

## Review Focus

These are the failure modes most likely to survive a happy-path implementation. Each is pinned by a test or an explicit verification step.

1. **A legacy database with every `sort_order = 0` moves a profile once and loses the new order on restart.** `reorder_profiles` must normalize the whole list in one transaction and reject duplicate ids; pinned by `reorder_profiles_persists_and_rolls_back_bad_lists`.
2. **Duplicating an auto-enabled profile silently doubles compression jobs.** The copy must start with `auto_enabled = false`; pinned by `duplicate_profile_inserts_named_copy_after_source`.
3. **A daemon enqueue races the delete confirmation and leaves an orphaned pending job.** Auto-enqueue must use `enqueue_job_for_existing_profile`, and `delete_profile` must re-check active jobs inside its transaction; pinned by `guarded_enqueue_skips_missing_profiles` and `delete_profile_blocks_active_jobs_and_keeps_history`.
4. **Auto-compression jobs enqueued in the same timestamp are consumed out of order.** `next_pending_job` must tie-break on rowid; pinned by `auto_jobs_follow_profile_sort_order`.
5. **Cancelling the delete dialog removes the profile anyway, or the last profile is deleted and the presets reappear on the next launch.** Cancel is a no-op, and both the UI and core refuse the last profile; Task 7 implements both and Task 8 verifies the block message in the demo.

---

### Task 1: Persist profile order in core

**Files:**
- Modify: `crates/core/src/db.rs:827-888` (`save_profile`, `insert_profile`, `ensure_default_profiles`)
- Test: `crates/core/src/db.rs` (`mod tests`, helper near line 1270 and tests near `ensure_default_profiles_is_idempotent` around line 1757)

**Interfaces:**
- Consumes: `CompressionProfile`, `Db::list_profiles() -> Result<Vec<CompressionProfile>>`.
- Produces: `Db::save_profile(&CompressionProfile) -> Result<()>` now inserts at the end and preserves the position on update.
- Produces: `Db::reorder_profiles(&self, ids: &[String]) -> Result<()>`.
- Produces (private): `fn next_sort_order(conn: &Connection) -> Result<i64>`, `fn write_profile(conn: &Connection, profile: &CompressionProfile, sort_order: i64) -> Result<()>`.

- [ ] **Step 1: Add the profile test helper**

Next to `photo_at` in the test module:

```rust
fn profile(id: &str, name: &str) -> CompressionProfile {
    CompressionProfile {
        id: id.to_string(),
        name: name.to_string(),
        ..CompressionProfile::default()
    }
}
```

- [ ] **Step 2: Write the failing tests**

Add after `ensure_default_profiles_does_not_seed_on_read_error`:

```rust
#[test]
fn save_profile_assigns_and_preserves_sort_order() {
    let db = Db::open_in_memory().unwrap();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        db.save_profile(&profile(id, name)).unwrap();
    }

    let ids = || {
        db.list_profiles()
            .unwrap()
            .into_iter()
            .map(|profile| profile.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(), vec!["a", "b", "c"]);

    let mut edited = profile("a", "A renamed");
    edited.quality = 44;
    db.save_profile(&edited).unwrap();

    assert_eq!(ids(), vec!["a", "b", "c"]);
    let stored = db.list_profiles().unwrap();
    assert_eq!(stored[0].name, "A renamed");
    assert_eq!(stored[0].quality, 44);
}

#[test]
fn reorder_profiles_persists_and_rolls_back_bad_lists() {
    let db = Db::open_in_memory().unwrap();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        db.save_profile(&profile(id, name)).unwrap();
    }

    let ids = || {
        db.list_profiles()
            .unwrap()
            .into_iter()
            .map(|profile| profile.id)
            .collect::<Vec<_>>()
    };
    let order = |values: &[&str]| values.iter().map(|id| id.to_string()).collect::<Vec<_>>();

    db.reorder_profiles(&order(&["c", "a", "b"])).unwrap();
    assert_eq!(ids(), vec!["c", "a", "b"]);

    assert!(matches!(
        db.reorder_profiles(&order(&["c", "a"])),
        Err(CoreError::Invalid(_))
    ));
    assert_eq!(ids(), vec!["c", "a", "b"]);

    assert!(matches!(
        db.reorder_profiles(&order(&["a", "a", "b"])),
        Err(CoreError::Invalid(_))
    ));
    assert_eq!(ids(), vec!["c", "a", "b"]);

    assert!(matches!(
        db.reorder_profiles(&order(&["c", "a", "missing"])),
        Err(CoreError::NotFound(_))
    ));
    assert_eq!(ids(), vec!["c", "a", "b"]);
}
```

Add `use crate::error::CoreError;` only if the test module does not already see it through `use super::*`; the production import in Step 4 puts it in scope for the child module.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core save_profile_assigns_and_preserves_sort_order reorder_profiles_persists_and_rolls_back_bad_lists`
Expected: FAIL with a missing `reorder_profiles` method (and the order assertions fail for insert-at-zero).

- [ ] **Step 4: Implement insert-at-end and reorder**

Replace `save_profile` / `insert_profile` with:

```rust
pub fn save_profile(&self, profile: &CompressionProfile) -> Result<()> {
    let sort_order = Self::next_sort_order(&self.conn)?;
    Self::write_profile(&self.conn, profile, sort_order)
}

fn next_sort_order(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(sort_order) + 1, 0) FROM compression_profiles",
        [],
        |row| row.get(0),
    )?)
}

fn write_profile(
    conn: &Connection,
    profile: &CompressionProfile,
    sort_order: i64,
) -> Result<()> {
    let json = serde_json::to_string(profile)?;
    conn.execute(
        "INSERT INTO compression_profiles(id, name, enabled, auto_enabled, json, sort_order) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, \
            auto_enabled = excluded.auto_enabled, json = excluded.json",
        params![
            profile.id,
            profile.name,
            profile.enabled as i64,
            profile.auto_enabled as i64,
            json,
            sort_order,
        ],
    )?;
    Ok(())
}
```

In `ensure_default_profiles`, seed with contiguous order:

```rust
for (index, profile) in CompressionProfile::defaults().into_iter().enumerate() {
    Self::write_profile(&transaction, &profile, index as i64)?;
}
```

Add `reorder_profiles` after `list_profiles`:

```rust
pub fn reorder_profiles(&self, ids: &[String]) -> Result<()> {
    let tx = self.conn.unchecked_transaction()?;
    let stored: i64 = tx.query_row(
        "SELECT COUNT(*) FROM compression_profiles",
        [],
        |row| row.get(0),
    )?;
    if stored != ids.len() as i64 {
        return Err(CoreError::Invalid(
            "profile order must list every profile".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            return Err(CoreError::Invalid(
                "profile order contains duplicate ids".to_string(),
            ));
        }
    }
    for (index, id) in ids.iter().enumerate() {
        let changed = tx.execute(
            "UPDATE compression_profiles SET sort_order = ?1 WHERE id = ?2",
            params![index as i64, id],
        )?;
        if changed != 1 {
            return Err(CoreError::NotFound(format!("compression profile {id}")));
        }
    }
    tx.commit()?;
    Ok(())
}
```

Import both collections in `db.rs`:

```rust
use std::collections::{HashMap, HashSet};
```

Import `CoreError` in `db.rs`:

```rust
use crate::error::{CoreError, Result};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core save_profile_assigns_and_preserves_sort_order reorder_profiles_persists_and_rolls_back_bad_lists`
Expected: PASS. Also run `cargo test -p vrc-photo-core ensure_default_profiles` and expect the existing seeding tests to pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat: persist compression profile order (#11)"
```

---

### Task 2: Duplicate a profile after its source

**Files:**
- Modify: `crates/core/src/db.rs` (after `list_profiles`, before `ensure_default_profiles`)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 1's `write_profile`, `list_profiles` ordering, `CompressionProfile`.
- Produces: `Db::duplicate_profile(&self, id: &str) -> Result<CompressionProfile>`.

- [ ] **Step 1: Write the failing tests**

Add after `reorder_profiles_persists_and_rolls_back_bad_lists`:

```rust
#[test]
fn duplicate_profile_inserts_named_copy_after_source() {
    let db = Db::open_in_memory().unwrap();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        db.save_profile(&profile(id, name)).unwrap();
    }
    let source = db
        .list_profiles()
        .unwrap()
        .into_iter()
        .find(|profile| profile.id == "b")
        .unwrap();

    let copy = db.duplicate_profile("b").unwrap();
    assert_ne!(copy.id, source.id);
    assert_eq!(copy.name, "B のコピー");
    assert!(!copy.auto_enabled);
    assert_eq!(copy.format, source.format);
    assert_eq!(copy.quality, source.quality);

    let stored = db.list_profiles().unwrap();
    let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", copy.id.as_str(), "c"]);

    let mut edited = copy.clone();
    edited.quality = 31;
    db.save_profile(&edited).unwrap();
    let stored = db.list_profiles().unwrap();
    assert_eq!(stored[1].quality, source.quality);
    assert_eq!(stored[2].quality, 31);

    let second = db.duplicate_profile("b").unwrap();
    assert_eq!(second.name, "B のコピー 2");
    let stored = db.list_profiles().unwrap();
    let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["a", "b", second.id.as_str(), copy.id.as_str(), "c"]
    );

    assert!(matches!(
        db.duplicate_profile("missing"),
        Err(CoreError::NotFound(_))
    ));
}

#[test]
fn duplicate_profile_normalizes_legacy_zero_order() {
    let db = Db::open_in_memory().unwrap();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        db.save_profile(&profile(id, name)).unwrap();
    }
    db.connection()
        .execute("UPDATE compression_profiles SET sort_order = 0", [])
        .unwrap();

    let copy = db.duplicate_profile("b").unwrap();
    let stored = db.list_profiles().unwrap();
    let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", copy.id.as_str(), "c"]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core duplicate_profile`
Expected: FAIL with a missing `duplicate_profile` method.

- [ ] **Step 3: Implement `duplicate_profile`**

```rust
pub fn duplicate_profile(&self, id: &str) -> Result<CompressionProfile> {
    let tx = self.conn.unchecked_transaction()?;
    let ordered = {
        let mut stmt = tx.prepare(
            "SELECT id, json FROM compression_profiles \
             ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut ordered = Vec::new();
        for row in rows {
            let (stored_id, json) = row?;
            ordered.push((stored_id, serde_json::from_str::<CompressionProfile>(&json)?));
        }
        ordered
    };

    let source_index = ordered
        .iter()
        .position(|(stored_id, _)| stored_id == id)
        .ok_or_else(|| CoreError::NotFound(format!("compression profile {id}")))?;

    let mut name = format!("{} のコピー", ordered[source_index].1.name);
    let mut suffix = 2;
    while ordered
        .iter()
        .any(|(_, profile)| profile.name == name)
    {
        name = format!("{} のコピー {suffix}", ordered[source_index].1.name);
        suffix += 1;
    }

    for (index, (stored_id, _)) in ordered.iter().enumerate() {
        tx.execute(
            "UPDATE compression_profiles SET sort_order = ?1 WHERE id = ?2",
            params![index as i64, stored_id],
        )?;
    }
    tx.execute(
        "UPDATE compression_profiles SET sort_order = sort_order + 1 WHERE sort_order > ?1",
        params![source_index as i64],
    )?;

    let copy = CompressionProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        auto_enabled: false,
        ..ordered[source_index].1.clone()
    };
    Self::write_profile(&tx, &copy, source_index as i64 + 1)?;
    tx.commit()?;
    Ok(copy)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core duplicate_profile`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat: duplicate compression profiles (#11)"
```

---

### Task 3: Guard deletion and enqueue, and make job order deterministic

**Files:**
- Modify: `crates/core/src/db.rs:880-914` (`delete_profile`, `enqueue_job`, `next_pending_job`)
- Test: `crates/core/src/db.rs` (`mod tests`)

**Interfaces:**
- Consumes: `QueueStatus`, `CompressionJob`, `Db::enqueue_job`, `Db::update_job`.
- Produces: `Db::active_job_count_for_profile(&self, profile_id: &str) -> Result<i64>`.
- Produces: `Db::delete_profile(&self, id: &str) -> Result<()>` now fails with `CoreError::Invalid` while active jobs exist or only one profile remains, and with `CoreError::NotFound` for an unknown id.
- Produces: `Db::enqueue_job_for_existing_profile(&self, photo_id: i64, profile_id: &str, policy: &str) -> Result<bool>`.

- [ ] **Step 1: Write the failing tests**

Replace the old placeholder-free test area after `duplicate_profile_normalizes_legacy_zero_order` with:

```rust
#[test]
fn delete_profile_blocks_active_jobs_and_keeps_history() {
    let db = Db::open_in_memory().unwrap();
    db.save_profile(&profile("p", "P")).unwrap();
    db.save_profile(&profile("q", "Q")).unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let job = db.enqueue_job(photo, "p", "keep").unwrap();

    assert_eq!(db.list_profiles().unwrap().len(), 2);
    assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
    assert!(matches!(
        db.delete_profile("p"),
        Err(CoreError::Invalid(_))
    ));
    assert_eq!(db.list_profiles().unwrap().len(), 2);

    db.update_job(&job, QueueStatus::Processing, 0.1, None, None)
        .unwrap();
    assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
    assert!(db.delete_profile("p").is_err());

    db.update_job(
        &job,
        QueueStatus::Completed,
        1.0,
        None,
        Some("/photos/compressed/a.webp"),
    )
    .unwrap();
    assert_eq!(db.active_job_count_for_profile("p").unwrap(), 0);

    db.delete_profile("p").unwrap();
    assert_eq!(db.list_profiles().unwrap().len(), 1);
    assert_eq!(db.list_profiles().unwrap()[0].id, "q");
    assert_eq!(db.list_jobs(10).unwrap().len(), 1);

    assert!(matches!(
        db.delete_profile("q"),
        Err(CoreError::Invalid(_))
    ));
    assert_eq!(db.list_profiles().unwrap().len(), 1);
    assert!(matches!(
        db.delete_profile("missing"),
        Err(CoreError::NotFound(_))
    ));
}

#[test]
fn guarded_enqueue_skips_missing_profiles() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();

    assert!(!db
        .enqueue_job_for_existing_profile(photo, "missing", "keep")
        .unwrap());
    assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 0);

    db.save_profile(&profile("p", "P")).unwrap();
    assert!(db
        .enqueue_job_for_existing_profile(photo, "p", "keep")
        .unwrap());
    assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core delete_profile_blocks_active_jobs_and_keeps_history guarded_enqueue_skips_missing_profiles`
Expected: FAIL with missing `active_job_count_for_profile` / `enqueue_job_for_existing_profile` methods.

- [ ] **Step 3: Implement the count, guarded delete, guarded enqueue, and rowid tie-break**

Replace `delete_profile` with:

```rust
pub fn active_job_count_for_profile(&self, profile_id: &str) -> Result<i64> {
    Ok(self.conn.query_row(
        "SELECT COUNT(*) FROM compression_jobs \
         WHERE profile_id = ?1 AND status IN ('pending', 'processing')",
        params![profile_id],
        |row| row.get(0),
    )?)
}

pub fn delete_profile(&self, id: &str) -> Result<()> {
    let tx = self.conn.unchecked_transaction()?;
    let exists: i64 = tx.query_row(
        "SELECT COUNT(*) FROM compression_profiles WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Err(CoreError::NotFound(format!("compression profile {id}")));
    }

    let active: i64 = tx.query_row(
        "SELECT COUNT(*) FROM compression_jobs \
         WHERE profile_id = ?1 AND status IN ('pending', 'processing')",
        params![id],
        |row| row.get(0),
    )?;
    if active > 0 {
        return Err(CoreError::Invalid(format!(
            "compression profile {id} has {active} active jobs"
        )));
    }

    let total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM compression_profiles",
        [],
        |row| row.get(0),
    )?;
    if total <= 1 {
        return Err(CoreError::Invalid(
            "at least one compression profile is required".to_string(),
        ));
    }

    let deleted = tx.execute(
        "DELETE FROM compression_profiles WHERE id = ?1",
        params![id],
    )?;
    if deleted == 0 {
        return Err(CoreError::NotFound(format!("compression profile {id}")));
    }
    tx.commit()?;
    Ok(())
}
```

Add `enqueue_job_for_existing_profile` after `enqueue_job`:

```rust
pub fn enqueue_job_for_existing_profile(
    &self,
    photo_id: i64,
    profile_id: &str,
    policy: &str,
) -> Result<bool> {
    let id = uuid::Uuid::new_v4().to_string();
    let inserted = self.conn.execute(
        "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, created_at) \
         SELECT ?1, ?2, ?3, 'pending', 0, ?4, ?5 \
         WHERE EXISTS (SELECT 1 FROM compression_profiles WHERE id = ?3)",
        params![id, photo_id, profile_id, policy, now_string()],
    )?;
    Ok(inserted == 1)
}
```

In `next_pending_job`, change the inner ordering to:

```sql
ORDER BY created_at, rowid LIMIT 1
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core delete_profile_blocks_active_jobs_and_keeps_history guarded_enqueue_skips_missing_profiles queue_roundtrip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat: guard compression profile deletion and enqueue (#11)"
```

---

### Task 4: Use the guarded enqueue and pin automatic-compression order

**Files:**
- Modify: `crates/daemon/src/lib.rs` (`enqueue_auto_jobs`, around 660)
- Test: `crates/daemon/src/lib.rs` (`mod tests`, after `end_to_end_auto_compress`)

**Interfaces:**
- Consumes: Task 1's `reorder_profiles`, Task 3's `next_pending_job` and `enqueue_job_for_existing_profile`, `enqueue_auto_jobs(&Db, &[i64])`.
- Produces: `enqueue_auto_jobs` skips profiles that disappeared after `list_profiles`; no signature change.

- [ ] **Step 1: Switch auto-enqueue to the existence-guarded insert**

In `enqueue_auto_jobs`, replace:

```rust
db.enqueue_job(*photo_id, &profile.id, "keep")?;
```

with:

```rust
db.enqueue_job_for_existing_profile(*photo_id, &profile.id, "keep")?;
```

The `bool` is intentionally ignored: a profile deleted between the list read and
this insert is skipped, and the delete transaction owns the error message.

- [ ] **Step 2: Add the regression test**

```rust
#[test]
fn auto_jobs_follow_profile_sort_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo_id = db
        .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
        .unwrap();

    let make = |id: &str, name: &str| CompressionProfile {
        id: id.to_string(),
        name: name.to_string(),
        auto_enabled: true,
        ..CompressionProfile::default()
    };
    let a = make("a", "A");
    let b = make("b", "B");
    let c = make("c", "C");
    for profile in [&a, &b, &c] {
        db.save_profile(profile).unwrap();
    }
    db.reorder_profiles(&[c.id.clone(), a.id.clone(), b.id.clone()])
        .unwrap();

    // One photo: its auto profiles are enqueued in the saved order.
    enqueue_auto_jobs(&db, &[photo_id]).unwrap();

    let mut order = Vec::new();
    while let Some(job) = db.next_pending_job().unwrap() {
        order.push(job.profile_id);
    }
    assert_eq!(order, vec![c.id, a.id, b.id]);
}
```

- [ ] **Step 3: Run the daemon tests**

Run: `cargo test -p vrc-photo-daemon auto_jobs_follow_profile_sort_order end_to_end_auto_compress`
Expected: PASS. If the sequence fails, confirm Task 3's `rowid` tie-break is present.

- [ ] **Step 4: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "test: pin auto compression order and guarded enqueue (#11)"
```

---

### Task 5: Add up/down controls to profile cards

**Files:**
- Modify: `crates/ui/src/main.rs` (helper near `step_index` around 5953; `App::move_profile` near `edit_profile` around 4409; card header around 4325-4370)
- Test: `crates/ui/src/main.rs` (`mod tests` around 6219)

**Interfaces:**
- Consumes: Task 1's `Db::reorder_profiles`, existing `step_index`.
- Produces: `fn moved_profile_order(ids: &[String], index: usize, delta: isize) -> Option<Vec<String>>`.
- Produces: `App::move_profile(&mut self, index: usize, delta: isize, cx: &mut Context<Self>)`.

- [ ] **Step 1: Write the failing helper test**

Add `moved_profile_order` to the explicit `use super::{...}` list in `mod tests`, then add:

```rust
#[test]
fn profile_move_order_swaps_neighbors_and_stops_at_edges() {
    let ids: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];

    assert_eq!(
        moved_profile_order(&ids, 1, -1),
        Some(vec!["b".to_string(), "a".to_string(), "c".to_string()])
    );
    assert_eq!(
        moved_profile_order(&ids, 1, 1),
        Some(vec!["a".to_string(), "c".to_string(), "b".to_string()])
    );
    assert_eq!(moved_profile_order(&ids, 0, -1), None);
    assert_eq!(moved_profile_order(&ids, 2, 1), None);
    assert_eq!(moved_profile_order(&[], 0, 1), None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-ui profile_move_order_swaps_neighbors_and_stops_at_edges`
Expected: FAIL with a missing `moved_profile_order` import.

- [ ] **Step 3: Implement the helper and the move method**

Next to `step_index`:

```rust
fn moved_profile_order(ids: &[String], index: usize, delta: isize) -> Option<Vec<String>> {
    if index >= ids.len() {
        return None;
    }
    let target = step_index(index, ids.len(), delta);
    if target == index {
        return None;
    }
    let mut order = ids.to_vec();
    order.swap(index, target);
    Some(order)
}
```

Near `edit_profile`:

```rust
fn move_profile(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
    let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
    let Some(order) = moved_profile_order(&ids, index, delta) else {
        return;
    };
    let target = step_index(index, ids.len(), delta);
    match self.db.reorder_profiles(&order) {
        Ok(()) => {
            self.profiles.swap(index, target);
            if self.profile_index == index {
                self.profile_index = target;
            } else if self.profile_index == target {
                self.profile_index = index;
            }
        }
        Err(error) => self.message = Some(format!("並び替えを保存できません: {error}")),
    }
    cx.notify();
}
```

- [ ] **Step 4: Add the arrows to each card**

In the card header, make the name take the remaining width, then add two 20×20 controls after the badge:

```rust
.child(
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_size(px(12.))
        .font_semibold()
        .text_color(rgb(if active { BLUE } else { MUTED }))
        .child(title),
)
.child(self.badge(
    if auto { "自動" } else { "手動" },
    if auto { GREEN } else { MUTED },
    if auto { 0x112a22 } else { PANEL_2 },
))
.child(
    div()
        .id(SharedString::from(format!("profile-up-{index}")))
        .w(px(20.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(11.))
        .text_color(rgb(if index == 0 { MUTED_2 } else { MUTED }))
        .when(index > 0, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(rgb(PANEL_2)).text_color(rgb(TEXT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.move_profile(index, -1, cx);
                    cx.stop_propagation();
                }))
        })
        .child("↑"),
)
.child(
    div()
        .id(SharedString::from(format!("profile-down-{index}")))
        .w(px(20.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(11.))
        .text_color(rgb(if index + 1 == len { MUTED_2 } else { MUTED }))
        .when(index + 1 < len, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(rgb(PANEL_2)).text_color(rgb(TEXT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.move_profile(index, 1, cx);
                    cx.stop_propagation();
                }))
        })
        .child("↓"),
)
```

Bind `let len = self.profiles.len();` before the `self.profiles.iter()` map so the closure does not re-borrow the list.

- [ ] **Step 5: Run the tests and clippy**

Run: `cargo test -p vrc-photo-ui profile_move_order_swaps_neighbors_and_stops_at_edges`
Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: move compression profiles with arrow buttons (#11)"
```

---

### Task 6: Duplicate from the profile editor

**Files:**
- Modify: `crates/ui/src/main.rs` (`App::duplicate_profile` near `move_profile`; editor header around 4428-4455)

**Interfaces:**
- Consumes: Task 2's `Db::duplicate_profile`, Task 5's selection handling.
- Produces: `App::duplicate_profile(&mut self, cx: &mut Context<Self>)`.

- [ ] **Step 1: Implement the method**

```rust
fn duplicate_profile(&mut self, cx: &mut Context<Self>) {
    let Some(profile) = self.profiles.get(self.profile_index) else {
        return;
    };
    let id = profile.id.clone();
    match self.db.duplicate_profile(&id) {
        Ok(copy) => {
            self.profiles = self.db.list_profiles().unwrap_or_default();
            self.profile_index = self
                .profiles
                .iter()
                .position(|profile| profile.id == copy.id)
                .unwrap_or(0);
            self.message = Some(format!(
                "「{}」を複製しました（自動適用はOFF）",
                copy.name
            ));
        }
        Err(error) => self.message = Some(format!("複製に失敗: {error}")),
    }
    cx.notify();
}
```

- [ ] **Step 2: Add the editor action**

In `profile_editor`'s header, before the auto `Switch`:

```rust
.child(
    Button::new("duplicate-profile")
        .label("複製")
        .small()
        .ghost()
        .on_click(cx.listener(|this, _, _, cx| this.duplicate_profile(cx))),
)
```

- [ ] **Step 3: Verify**

Run: `cargo check -p vrc-photo-ui` and `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`.
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: duplicate compression profile from the editor (#11)"
```

---

### Task 7: Confirm and guard profile deletion

**Files:**
- Modify: `crates/ui/src/main.rs` (imports near line 11; methods near `duplicate_profile`; editor header around 4428-4460)

**Interfaces:**
- Consumes: Task 3's `Db::active_job_count_for_profile` and `Db::delete_profile`; `gpui-component`'s `AlertDialog` and `WindowExt`.
- Produces: `App::request_delete_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>)`.
- Produces: `App::delete_profile(&mut self, id: &str, cx: &mut Context<Self>)`.

- [ ] **Step 1: Update the button import**

```rust
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
```

`AlertDialog` and `WindowExt` are already re-exported through `use gpui_kit::component::*;`.

- [ ] **Step 2: Implement the pre-check, dialog, and delete**

```rust
fn request_delete_profile(
    &mut self,
    id: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
) {
    let Some(profile) = self
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
    else {
        return;
    };
    if self.profiles.len() <= 1 {
        self.message = Some("最後のプロファイルは削除できません".to_string());
        cx.notify();
        return;
    }
    match self.db.active_job_count_for_profile(id) {
        Ok(0) => {
            let entity = cx.entity();
            let id = id.to_string();
            let name = profile.name.clone();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let entity = entity.clone();
                let id = id.clone();
                alert
                    .title("プロファイルを削除")
                    .description(format!(
                        "「{name}」を削除します。この操作は元に戻せません。"
                    ))
                    .confirm()
                    .ok_text("削除")
                    .ok_variant(ButtonVariant::Danger)
                    .cancel_text("キャンセル")
                    .on_ok(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.delete_profile(&id, cx));
                        true
                    })
            });
        }
        Ok(count) => {
            self.message = Some(format!(
                "「{}」は未完了のジョブが{}件あるため削除できません",
                profile.name, count
            ));
            cx.notify();
        }
        Err(error) => {
            self.message = Some(format!("削除前の確認に失敗: {error}"));
            cx.notify();
        }
    }
}

fn delete_profile(&mut self, id: &str, cx: &mut Context<Self>) {
    let removed_index = self
        .profiles
        .iter()
        .position(|profile| profile.id == id)
        .unwrap_or(self.profile_index);
    let name = self
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| id.to_string());
    match self.db.delete_profile(id) {
        Ok(()) => {
            self.profiles = self.db.list_profiles().unwrap_or_default();
            self.profile_index = removed_index.min(self.profiles.len().saturating_sub(1));
            self.message = Some("プロファイルを削除しました".to_string());
        }
        Err(error) => {
            let message = match self.db.active_job_count_for_profile(id) {
                Ok(count) if count > 0 => format!(
                    "「{name}」は未完了のジョブが{count}件あるため削除できません"
                ),
                _ if self.profiles.len() <= 1 => {
                    "最後のプロファイルは削除できません".to_string()
                }
                _ => format!("プロファイルを削除できません: {error}"),
            };
            self.message = Some(message);
        }
    }
    cx.notify();
}
```

- [ ] **Step 3: Add the editor delete action**

At the top of `profile_editor`, add:

```rust
let profile_id = profile.id.clone();
```

Then in the header, after the duplicate button and before the auto `Switch`:

```rust
.child(
    Button::new("delete-profile")
        .label("削除")
        .small()
        .danger()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.request_delete_profile(&profile_id, window, cx);
        })),
)
```

- [ ] **Step 4: Verify**

Run: `cargo check -p vrc-photo-ui` and `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`.
Expected: PASS, including the `cx.entity()` and `AlertDialog` callback types.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: confirm and guard compression profile deletion (#11)"
```

---

### Task 8: Full verification and demo pass

**Files:**
- No production files expected. If verification finds a defect, fix it in the owning file and amend the relevant commit or add a `fix:` commit.

**Interfaces:**
- Consumes: all tasks.
- Produces: a clean branch ready for PR.

- [ ] **Step 1: Run the full local gate**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Expected: all commands exit 0. Record the new test counts for the PR body.

- [ ] **Step 2: Demo the reorder and restart persistence**

```bash
cargo run -p vrc-photo-ui -- --demo --demo-reset
```

- On 圧縮設定, move a profile down and another up; the selected profile follows its card.
- Quit and relaunch `cargo run -p vrc-photo-ui -- --demo`.
- Confirm the order is the same and the edge arrows are disabled at the top/bottom.

- [ ] **Step 3: Demo duplication**

- Select a profile and click 複製.
- Confirm the copy appears directly below the source, its name is `... のコピー`, and the auto badge is 手動.
- Change the copy's quality; select the source and confirm its quality did not change.

- [ ] **Step 4: Demo delete confirmation**

- Click 削除, then キャンセル; the profile remains and the selection does not change.
- Click 削除 again and confirm; the profile disappears and the next profile is selected.
- Delete until one profile remains, then click 削除; expect
  「最後のプロファイルは削除できません」 and the profile remains. The presets
  must not reappear on the next launch.

- [ ] **Step 5: Demo the active-job guard**

- Select a photo and click 手動圧縮 with a profile selected.
- Immediately try to delete that profile; expect the message
  `「...」は未完了のジョブが...件あるため削除できません` and no dialog.
- Wait for the job to complete (or let it fail) and confirm deletion is then allowed.

- [ ] **Step 6: Push and open the code PR**

```bash
git push -u origin feat/issue-11-profile-management
gh pr create --title "feat: 圧縮プロファイル管理（並び替え・複製・削除確認） (#11)" \
  --body "Closes #11

## 変更
- プロファイルの並び順を \`sort_order\` に保存し、自動圧縮の適用順に反映
- 一覧カードの ↑↓ で並び替え
- 「〈名前〉のコピー」を元の直後に複製（自動適用はOFF）
- 削除確認の AlertDialog と、未完了ジョブ・最後の1件の削除ブロック
- 自動enqueueはプロファイル存在を確認して挿入し、削除との競合で孤児ジョブを作らない
- 完了済みジョブ・derivative は履歴として保持

## 検証
- cargo fmt --all -- --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --workspace
- デモで並び替え・再起動・複製・削除キャンセル/確定・ジョブ保護を確認"
```

Expected: the PR is created. Do not merge until CI is green (AGENTS §5-§6).
